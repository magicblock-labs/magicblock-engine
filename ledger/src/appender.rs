//! Ledger append service and writable superblock storage.

use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering::*},
    thread::JoinHandle,
};

use agave_transaction_view::transaction_view::TransactionView;
use bitcode::Buffer;
use flume::Receiver;
use nucleus::{Slot, ledger::BlockstorePosition};
use solana_signature::Signature;
use tokio::sync::broadcast::Sender;
use tracing::{info, warn};
use wincode::Error;
use zstd::bulk::Compressor;

use crate::{
    Ledger, Superblock, codec,
    error::{LedgerError, Result},
    index::{IndexWriter, Span, TxSpan},
    metrics::{self, Operation},
    schema::{
        Block, BlockstoreEntry, Event, Execution, ExecutionDetails, MAX_EXECUTION_DETAILS_SIZE,
        SuperblockSeal, TransactionEntry, blockstore,
    },
    storage::{AppendFile, Durability},
};

/// Blockstore stream file name inside a superblock.
pub(crate) const BLOCKSTORE_DB: &str = "blockstore.db";
/// Execution details file name inside a superblock.
pub(crate) const EXECUTIONS_DB: &str = "executions.db";
/// Superblock metadata file name.
pub(crate) const SUPERBLOCK_META: &str = "superblock.meta";
/// Frequency of ledger size checks in slots.
pub(crate) const SIZE_CHECK_FREQUENCY: u64 = 128;

/// Background service that appends ledger events into the active superblock.
pub(crate) struct LedgerAppender {
    /// Shared top-level ledger state.
    ledger: Arc<Ledger>,
    /// Writable files for the active superblock.
    writer: SuperblockWriter,
    /// Transactions waiting for their matching execution details.
    pending: HashMap<Signature, PendingTx>,
    /// Active superblock index writer and pending atomic boundary.
    index: IndexWriter,
    /// Outstanding physical cleanup for the last truncated superblock.
    truncation: Option<JoinHandle<Result<()>>>,
    /// Event stream from the execution pipeline.
    rx: Receiver<Event>,
    /// Transactions written since the last published boundary.
    transactions: u64,
    /// Broadcasts the blockstore write position after each committed block.
    position: Sender<BlockstorePosition>,
}

impl LedgerAppender {
    /// Opens the active superblock and processes events until the stream closes.
    pub(crate) fn run(
        ledger: Arc<Ledger>,
        rx: Receiver<Event>,
        position: Sender<BlockstorePosition>,
    ) -> Result<()> {
        let head = ledger.meta.head();
        metrics::pending_transactions(0);
        let superblock = ledger
            .superblocks
            .read()
            .get(&head)
            .cloned()
            .ok_or(LedgerError::Corruption("active superblock missing"))?;
        let index = ledger.index.writer(&superblock.index);
        let writer = SuperblockWriter::new(superblock)?;

        let mut appender = Self {
            ledger,
            writer,
            index,
            truncation: None,
            rx,
            pending: HashMap::new(),
            transactions: 0,
            position,
        };

        let result = appender.serve();
        let truncation = appender.join_truncation();
        result.and(truncation)
    }

    /// Processes append events until all senders close or a final sync arrives.
    fn serve(&mut self) -> Result<()> {
        while let Ok(event) = self.rx.recv() {
            match event {
                Event::Transaction(transaction) => self.write_transaction(transaction)?,
                Event::Execution(execution) => self.write_execution(execution)?,
                Event::Block(block) => self.write_block(block)?,
                Event::Superblock { seal, response } => {
                    self.seal(seal, false)?;
                    let _ = response.send(());
                }
                Event::Bootstrap(seal) => self.seal(seal, true)?,
                Event::Reset(slot) => self.write_reset(slot)?,
                Event::Sync { response, is_final } => {
                    self.sync(None)?;
                    let _ = response.send(());
                    if is_final {
                        return Ok(());
                    }
                }
            }
        }
        self.sync(None)
    }

    /// Rotates to the next superblock directory.
    fn rotate(&mut self, seal: SuperblockSeal) -> Result<()> {
        let _timer = metrics::time(Operation::Rotate);
        let head = seal.id + 1;
        let superblock = Superblock::open(&self.ledger.directory, head, &self.ledger.index)?;
        // Seal N opens N+1, which stores N's snapshot archive and seal metadata.
        superblock.meta.checksum.store(seal.checksum, Release);
        superblock.meta.transactions.store(seal.transactions, Release);
        superblock.meta.flush()?;
        let writer = SuperblockWriter::new(superblock.clone())?;
        let index = self.ledger.index.writer(&superblock.index);

        let mut superblocks = self.ledger.superblocks.write();
        superblocks.insert(head, superblock);
        self.ledger.meta.head.store(head, Release);
        self.ledger.meta.superblocks.fetch_add(1, Release);
        self.ledger.meta.flush()?;
        drop(superblocks);

        self.writer = writer;
        self.index = index;
        let position = BlockstorePosition { superblock: head, offset: 0 };
        let _ = self.position.send(position);
        info!(head, "opened active superblock");
        Ok(())
    }

    /// Seals the active superblock, optionally adopting a restored snapshot's
    /// cumulative transaction count before publishing the successor metadata.
    fn seal(&mut self, seal: SuperblockSeal, bootstrap: bool) -> Result<()> {
        self.write_superblock(seal)?;
        if bootstrap {
            self.ledger.meta.transactions.store(seal.transactions, Release);
        }
        self.rotate(seal)
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
    fn write_execution(&mut self, execution: Execution) -> Result<()> {
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
        self.index.insert_transaction(&signature, span);
        let view = TransactionView::try_new_unsanitized(pending.transaction)?;
        let accounts = view.static_account_keys();
        self.index.insert_accounts(accounts, span.execution);
        Ok(())
    }

    /// Writes a block boundary and publishes it after data and indexes reach the OS.
    fn write_block(&mut self, block: Block) -> Result<()> {
        let span = self.writer.write_blockstore(&BlockstoreEntry::Block(block))?;
        self.index.insert_block(block.slot, span);
        self.publish(Some(block.slot), Durability::Buffer)?;
        if block.slot.is_multiple_of(SIZE_CHECK_FREQUENCY) && self.ledger.size_exceeded()? {
            self.sync(None)?;
            self.truncate()?;
        }
        metrics::ledger_counts(&self.ledger);
        Ok(())
    }

    /// Joins the previous cleanup before starting another truncation worker.
    fn truncate(&mut self) -> Result<()> {
        self.join_truncation()?;
        self.truncation = self.ledger.truncate()?;
        Ok(())
    }

    /// Joins and clears the outstanding truncation worker, if any.
    fn join_truncation(&mut self) -> Result<()> {
        let Some(worker) = self.truncation.take() else { return Ok(()) };
        worker.join().map_err(|_| LedgerError::TruncationPanic)?
    }

    /// Writes a superblock seal and prepares files for read-only access.
    fn write_superblock(&mut self, seal: SuperblockSeal) -> Result<()> {
        self.writer.write_blockstore(&BlockstoreEntry::Superblock(seal))?;
        self.sync(None)?;
        self.writer.finalize()?;
        self.index.rotate_memtable()?;
        info!(superblock = seal.id, "sealed superblock");
        Ok(())
    }

    /// Writes and publishes a volatile-state reset marker.
    fn write_reset(&mut self, slot: Slot) -> Result<()> {
        self.writer.write_blockstore(&BlockstoreEntry::Reset(slot))?;
        self.sync(None)?;
        info!(slot, "appended volatile state reset");
        Ok(())
    }

    /// Makes files and indexes durable, publishes their cursors and accumulated
    /// transaction count, and broadcasts the new blockstore position. When
    /// `slot` is supplied, the same boundary also publishes block metadata.
    fn sync(&mut self, slot: Option<Slot>) -> Result<()> {
        self.publish(slot, Durability::SyncData)
    }

    /// Publishes one complete boundary with buffered or data-synced durability.
    fn publish(&mut self, slot: Option<Slot>, durability: Durability) -> Result<()> {
        let cursors = self.writer.persist(durability)?;
        self.index.persist(durability)?;
        self.writer.publish(cursors, slot, durability)?;
        self.ledger.meta.transactions.fetch_add(self.transactions, Release);
        if let Some(slot) = slot {
            self.ledger.meta.blocks.fetch_add(1, Release);
            self.ledger.meta.range.end.store(slot, Release);
        }
        self.ledger.meta.persist(durability)?;
        self.transactions = 0;
        let position = BlockstorePosition {
            superblock: self.ledger.meta.head(),
            offset: cursors.blockstore,
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

/// Published byte cursors for the active superblock data files.
#[derive(Clone, Copy)]
struct Cursors {
    blockstore: u64,
    executions: u64,
}

/// Writable superblock files and reusable execution-detail encoder.
struct SuperblockWriter {
    /// Active superblock whose metadata publishes these files.
    superblock: Arc<Superblock>,
    /// Compressor reused for execution metadata payloads.
    compressor: Compressor<'static>,
    /// Scratch buffer owned by bitcode while encoding metadata.
    buffer: Buffer,
    /// Buffered transaction/block/seal blockstore stream.
    blockstore: AppendFile,
    /// Buffered execution details stream.
    executions: AppendFile,
}

impl SuperblockWriter {
    /// Opens writable files for the active `superblock`.
    fn new(superblock: Arc<Superblock>) -> Result<Self> {
        let directory = &superblock.directory;
        Ok(Self {
            blockstore: AppendFile::new(
                &directory.join(BLOCKSTORE_DB),
                &superblock.meta.cursors.blockstore,
            )?,
            executions: AppendFile::new(
                &directory.join(EXECUTIONS_DB),
                &superblock.meta.cursors.executions,
            )?,
            superblock,
            compressor: codec::compressor()?,
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

    /// Persists data files and returns their published cursors.
    fn persist(&mut self, durability: Durability) -> Result<Cursors> {
        let _timer = if durability.requires_sync() {
            metrics::time(Operation::FileSync)
        } else {
            metrics::time(Operation::BufferSync)
        };
        Ok(Cursors {
            blockstore: self.blockstore.persist(durability)?,
            executions: self.executions.persist(durability)?,
        })
    }

    /// Publishes file cursors into superblock metadata at the selected durability.
    fn publish(&self, cursors: Cursors, slot: Option<u64>, durability: Durability) -> Result<()> {
        let metadata = &self.superblock.meta;
        metadata.cursors.blockstore.store(cursors.blockstore, Release);
        metadata.cursors.executions.store(cursors.executions, Release);
        if let Some(slot) = slot {
            metadata.range.end.store(slot, Release);
            // The first block of a segment fixes its start
            // slot; later blocks only extend the end.
            let _ = metadata.range.start.compare_exchange(0, slot, Release, Relaxed);
        }
        metadata.persist(durability)
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
