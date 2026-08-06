//! Ledger append service and writable superblock storage.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, atomic::Ordering::*},
};

use agave_transaction_view::transaction_view::TransactionView;
use bitcode::Buffer;
use flume::Receiver;
use nucleus::{
    Slot,
    heed::{DatabaseIndex, OptRwTxn},
    ledger::BlockstorePosition,
    shutdown::{ShutdownHandle, ShutdownReason},
};
use solana_signature::Signature;
use tokio::sync::broadcast::Sender;
use tracing::{info, warn};
use wincode::Error;
use zstd::bulk::Compressor;

use crate::{
    Ledger, Superblock,
    error::{LedgerError, Result},
    index::{Index, Span, TxSpan},
    metrics::{self, Operation},
    schema::{
        Block, BlockstoreEntry, Event, Execution, ExecutionDetails, MAX_EXECUTION_DETAILS_SIZE,
        SuperblockSeal, TransactionEntry, blockstore,
    },
    storage::{AppendFile, MetaMap, SuperblockMeta},
};

/// Blockstore stream file name inside a superblock.
pub(crate) const BLOCKSTORE_DB: &str = "blockstore.db";
/// Execution details file name inside a superblock.
pub(crate) const EXECUTIONS_DB: &str = "executions.db";
/// Superblock metadata file name.
pub(crate) const SUPERBLOCK_META: &str = "superblock.meta";

/// Background service that appends ledger events into the active superblock.
pub(crate) struct LedgerAppender {
    /// Shared top-level ledger state.
    ledger: Arc<Ledger>,
    /// Writable files for the active superblock.
    writer: SuperblockWriter,
    /// Transactions waiting for their matching execution details.
    pending: HashMap<Signature, PendingTx>,
    /// Active superblock index.
    index: Arc<Index>,
    /// Event stream from the execution pipeline.
    rx: Receiver<Event>,
    /// Transactions written since the last committed block boundary.
    transactions: u64,
    /// Broadcasts the blockstore write position after each committed block.
    position: Sender<BlockstorePosition>,
}

impl LedgerAppender {
    /// Opens the active superblock and registers it on the ledger handle.
    pub(crate) fn new(
        ledger: Arc<Ledger>,
        rx: Receiver<Event>,
        position: Sender<BlockstorePosition>,
    ) -> Result<Self> {
        let head = ledger.meta.head();
        let directory = Superblock::init_dir(&ledger.directory, head)?;
        let writer = SuperblockWriter::new(&directory)?;
        metrics::pending_transactions(0);
        let index = ledger
            .superblocks
            .read()
            .get(&head)
            .map(|s| s.index.clone())
            .ok_or(LedgerError::Corruption("active superblock missing"))?;

        Ok(Self {
            ledger,
            writer,
            index,
            rx,
            pending: HashMap::new(),
            transactions: 0,
            position,
        })
    }

    /// Runs until shutdown (when the event stream closes).
    pub(crate) fn run(mut self, mut shutdown: ShutdownHandle) {
        let mut txn = None;
        let reason = loop {
            let Ok(event) = self.rx.recv() else {
                break ShutdownReason::Signalled;
            };
            match self.process(event, &mut txn) {
                Ok(false) => (),
                Ok(true) => break ShutdownReason::Signalled,

                Err(err) => break ShutdownReason::Error(Box::new(err)),
            }
        };
        // Release ledger ownership before the manager can reopen it.
        drop(txn);
        drop(self);
        shutdown.terminate(reason);
    }

    /// Rotates to the next superblock directory.
    fn rotate(&mut self, seal: SuperblockSeal) -> Result<()> {
        let _timer = metrics::time(Operation::Rotate);
        let head = seal.id + 1;
        let superblock = Superblock::open(&self.ledger.directory, head)?;
        // Seal N opens N+1, which stores N's snapshot archive and checksum.
        superblock.meta.checksum.store(seal.checksum, Release);
        self.writer = SuperblockWriter::new(&superblock.directory)?;
        self.ledger.meta.head.store(head, Release);
        self.ledger.meta.superblocks.fetch_add(1, Release);
        self.ledger.meta.flush()?;
        self.index = superblock.index.clone();
        superblock.meta.flush()?;
        self.ledger.superblocks.write().insert(head, superblock);
        info!(head, "opened active superblock");
        Ok(())
    }

    /// Processes one append event, rotating after a superblock seal.
    ///
    /// Returns true if the shutdown has been requested.
    fn process(&mut self, event: Event, txn: OptRwTxn<'_, '_>) -> Result<bool> {
        match event {
            Event::Transaction(transaction) => {
                self.write_transaction(transaction)?;
            }
            Event::Execution(execution) => {
                self.write_execution(execution, txn)?;
            }
            Event::Block(block) => {
                self.write_block(block, txn)?;
            }
            Event::Superblock(seal) => {
                self.write_superblock(seal)?;
                self.rotate(seal)?;
            }
            Event::Reset(slot) => {
                self.write_reset(slot)?;
            }
            Event::Sync { response, is_final } => {
                let _ = response.send(self.sync(None, txn));
                return Ok(is_final);
            }
        }
        Ok(false)
    }

    /// Writes a raw transaction and keeps it pending until execution arrives.
    fn write_transaction(&mut self, transaction: TransactionEntry) -> Result<()> {
        let entry = BlockstoreEntry::Transaction(transaction.payload.as_slice());
        let span = self.writer.write_blockstore(&entry)?;
        let entry = PendingTx {
            transaction: transaction.payload,
            span,
        };
        self.pending.insert(transaction.signature, entry);
        metrics::pending_transactions(self.pending.len());
        self.transactions += 1;
        Ok(())
    }

    /// Writes execution details and adds transaction/account indexes.
    fn write_execution(&mut self, execution: Execution, txn: OptRwTxn<'_, '_>) -> Result<()> {
        let signature: Signature = execution.header.signature;
        let Some(pending) = self.pending.remove(&signature) else {
            warn!(%signature, "ledger execution arrived without a pending transaction; skipping");
            return Ok(());
        };
        metrics::pending_transactions(self.pending.len());
        let execution = self.writer.write_execution(&execution)?;
        let span = TxSpan {
            blockstore: pending.span,
            execution,
        };
        self.index.insert_transaction(txn, &signature, &span)?;
        let view = TransactionView::try_new_unsanitized(pending.transaction)?;
        let accounts = view.static_account_keys();
        self.index.insert_accounts(txn, accounts, &span.execution)?;
        Ok(())
    }

    /// Writes a block boundary and publishes it after data and indexes are durable.
    fn write_block(&mut self, block: Block, txn: OptRwTxn<'_, '_>) -> Result<()> {
        let span = self.writer.write_blockstore(&BlockstoreEntry::Block(block))?;
        self.index.insert_block(txn, &block.slot, &span)?;
        self.sync(Some(block.slot), txn)?;
        self.index.flush()?;
        self.ledger.meta.transactions.fetch_add(self.transactions, Release);
        self.ledger.meta.blocks.fetch_add(1, Release);
        self.ledger.meta.range.end.store(block.slot, Release);
        self.ledger.meta.flush()?;
        if self.ledger.size_exceeded()? {
            self.ledger.truncate()?;
        }
        metrics::ledger_counts(&self.ledger);
        self.transactions = 0;
        Ok(())
    }

    /// Writes a superblock seal and prepares files for read-only access.
    fn write_superblock(&mut self, seal: SuperblockSeal) -> Result<()> {
        self.writer.write_blockstore(&BlockstoreEntry::Superblock(seal))?;
        self.sync(None, &mut None)?;
        self.writer.finalize()?;
        info!(superblock = seal.id, "sealed superblock");
        Ok(())
    }

    /// Writes and publishes a volatile-state reset marker.
    fn write_reset(&mut self, slot: Slot) -> Result<()> {
        self.writer.write_blockstore(&BlockstoreEntry::Reset(slot))?;
        self.sync(None, &mut None)?;
        info!(slot, "appended volatile state reset");
        Ok(())
    }

    /// Syncs data files, republishes durable cursors (extending the segment slot
    /// range when `slot` is given), and broadcasts the new blockstore position.
    fn sync(&mut self, slot: Option<Slot>, txn: OptRwTxn<'_, '_>) -> Result<()> {
        let cursors = self.writer.sync()?;
        self.writer.publish(cursors, slot)?;
        if let Some(txn) = txn.take() {
            txn.commit()?;
        }
        let position = BlockstorePosition {
            superblock: self.ledger.meta.head(),
            offset: cursors.0,
        };
        let _ = self.position.send(position);
        Ok(())
    }
}

/// Transaction bytes already written but not yet paired with execution details.
struct PendingTx {
    /// Transaction bytes retained until execution details arrive for indexing.
    transaction: Arc<Vec<u8>>,
    /// Blockstore-file span of the transaction bytes.
    span: Span,
}

/// Writable superblock files and reusable execution-detail encoder.
struct SuperblockWriter {
    /// Compressor reused for execution metadata payloads.
    compressor: Compressor<'static>,
    /// Scratch buffer owned by bitcode while encoding metadata.
    buffer: Buffer,
    /// Buffered transaction/block/seal blockstore stream.
    blockstore: AppendFile,
    /// Buffered execution details stream.
    executions: AppendFile,
    /// Mmap-backed metadata for this superblock.
    metadata: MetaMap<SuperblockMeta>,
}

impl SuperblockWriter {
    /// Opens writable files and metadata under `directory`.
    fn new(directory: &Path) -> Result<Self> {
        // SAFETY: `SuperblockMeta` and its nested headers have stable C layouts,
        // and all fields that can change while mapped are atomic. The ledger
        // exclusively creates and updates this superblock metadata file.
        let metadata = unsafe { MetaMap::<SuperblockMeta>::new(&directory.join(SUPERBLOCK_META)) }?;

        Ok(Self {
            blockstore: AppendFile::new(
                &directory.join(BLOCKSTORE_DB),
                &metadata.cursors.blockstore,
            )?,
            executions: AppendFile::new(
                &directory.join(EXECUTIONS_DB),
                &metadata.cursors.executions,
            )?,
            metadata,
            compressor: Compressor::new(0)?,
            buffer: Buffer::new(),
        })
    }

    /// Appends an entry to the blockstore file.
    fn write_blockstore(&mut self, entry: &BlockstoreEntry<&[u8]>) -> Result<Span> {
        let offset = self.blockstore.cursor;
        blockstore::encode(&mut self.blockstore, entry).map_err(Into::<Error>::into)?;
        let size = self.blockstore.cursor - offset;
        Ok(Span::new(offset, size))
    }

    /// Appends execution details and returns their file span.
    fn write_execution(&mut self, execution: &Execution) -> Result<Span> {
        let offset = self.executions.cursor;
        wincode::serialize_into(&mut self.executions, &execution.header)
            .map_err(Into::<Error>::into)?;
        let mut details = self.buffer.encode(&execution.details);
        if details.len() > MAX_EXECUTION_DETAILS_SIZE {
            // Omit oversized details but retain the fixed execution header and status.
            details = self.buffer.encode(&None::<ExecutionDetails>);
        }
        self.executions.compress(details, &mut self.compressor)?;
        let size = self.executions.cursor - offset;
        Ok(Span::new(offset, size))
    }

    /// Syncs data files and returns durable cursors.
    fn sync(&mut self) -> Result<(u64, u64)> {
        let _timer = metrics::time(Operation::FileSync);
        Ok((self.blockstore.sync()?, self.executions.sync()?))
    }

    /// Publishes durable cursors into superblock metadata.
    fn publish(&self, cursors: (u64, u64), slot: Option<u64>) -> Result<()> {
        let (blockstore, executions) = cursors;
        self.metadata.cursors.blockstore.store(blockstore, Release);
        self.metadata.cursors.executions.store(executions, Release);
        if let Some(slot) = slot {
            self.metadata.range.end.store(slot, Release);
            // The first block of a segment fixes its start
            // slot; later blocks only extend the end.
            let _ = self.metadata.range.start.compare_exchange(0, slot, Release, Relaxed);
        }
        self.metadata.flush()?;
        Ok(())
    }

    /// Trims preallocated file space after the superblock cursors are durable.
    fn finalize(&mut self) -> Result<()> {
        let _timer = metrics::time(Operation::FileFinalize);
        self.blockstore.finalize()?;
        self.executions.finalize()?;
        info!(
            blockstore = self.blockstore.cursor,
            executions = self.executions.cursor,
            "sealed superblock files"
        );
        Ok(())
    }
}
