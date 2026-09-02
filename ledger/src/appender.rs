//! Ledger append service and writable superblock storage.

use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering::*},
    thread::JoinHandle,
};

use bitcode::Buffer;
use flume::Receiver;
use nucleus::{Slot, ledger::BlockstorePosition};
use oneshot::Sender as Response;
use solana_signature::Signature;
use tokio::sync::broadcast::Sender;
use tracing::{info, warn};
use wincode::Error;
use zstd::bulk::Compressor;

use crate::{
    Ledger, Superblock, codec,
    error::{LedgerError, Result},
    index::{Span, TxSpan},
    indexer::{IndexMessage, IndexerHandle},
    metrics::{self, Operation},
    schema::{
        AccountIndex, Block, BlockstoreEntry, Event, Execution, ExecutionDetails,
        MAX_EXECUTION_DETAILS_SIZE, SuperblockSeal, TransactionEntry, blockstore,
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
    pending: HashMap<Signature, Span>,
    /// Sole sender for ordered background indexing.
    indexer: IndexerHandle,
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
        indexer: IndexerHandle,
    ) -> Result<()> {
        let head = ledger.meta.head();
        metrics::pending_transactions(0);
        let superblock = ledger
            .superblocks
            .read()
            .get(&head)
            .cloned()
            .ok_or(LedgerError::Corruption("active superblock missing"))?;
        let writer = SuperblockWriter::new(superblock)?;

        let mut appender = Self {
            ledger,
            writer,
            indexer,
            truncation: None,
            rx,
            pending: HashMap::new(),
            transactions: 0,
            position,
        };

        let result = loop {
            let Ok(event) = appender.rx.recv() else { break appender.sync() };
            let result = match event {
                Event::Transaction(transaction) => appender.write_transaction(transaction),
                Event::Execution { execution, accounts } => {
                    appender.write_execution(execution, accounts)
                }
                Event::Block(block) => appender.write_block(block),
                Event::Superblock { seal, response } => {
                    let result = appender.seal(seal, false);
                    acknowledge(&result, response);
                    result
                }
                Event::Bootstrap(seal) => appender.seal(seal, true),
                Event::Reset(slot) => appender.write_reset(slot),
                Event::Sync { response, is_final } => {
                    let result = appender.sync();
                    acknowledge(&result, response);
                    if is_final {
                        break result;
                    }
                    result
                }
            };
            if let Err(error) = result {
                break Err(error);
            }
        };
        let truncation = appender.join_truncation();
        result.and(truncation)
    }

    /// Rotates to the next superblock directory.
    fn rotate(&mut self, seal: SuperblockSeal) -> Result<()> {
        let _timer = metrics::time(Operation::Rotate);
        let head = seal.id + 1;
        let meta = Superblock::open_meta(&self.ledger.directory, head)?;
        let superblock = Superblock::open(meta, &self.ledger.index)?;
        // Seal N opens N+1, which stores N's snapshot archive and seal metadata.
        superblock.meta.checksum.store(seal.checksum, Release);
        superblock.meta.transactions.store(seal.transactions, Release);
        superblock.meta.flush()?;
        let writer = SuperblockWriter::new(superblock.clone())?;
        let keyspace = superblock.index.clone();

        let mut superblocks = self.ledger.superblocks.write();
        superblocks.insert(head, superblock);
        self.ledger.meta.head.store(head, Release);
        self.ledger.meta.superblocks.fetch_add(1, Release);
        self.ledger.meta.flush()?;
        drop(superblocks);

        self.writer = writer;
        self.indexer.fence(Some(keyspace))?;
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
        self.pending.insert(transaction.signature, span);
        metrics::pending_transactions(self.pending.len());
        self.transactions += 1;
        Ok(())
    }

    /// Writes execution details and queues their authoritative spans for indexing.
    fn write_execution(&mut self, execution: Execution, accounts: AccountIndex) -> Result<()> {
        let signature: Signature = execution.header.signature;
        let Some(blockstore) = self.pending.remove(&signature) else {
            warn!(%signature, "ledger execution arrived without a pending transaction; skipping");
            return Ok(());
        };
        metrics::pending_transactions(self.pending.len());
        let execution_span = self.writer.write_execution(&execution)?;
        let span = TxSpan {
            blockstore,
            execution: execution_span,
        };
        self.indexer.send(IndexMessage::Transaction { signature, accounts, span })
    }

    /// Publishes a block's data spans before queuing its atomic index commit.
    fn write_block(&mut self, block: Block) -> Result<()> {
        let span = self.writer.write_blockstore(&BlockstoreEntry::Block(block))?;
        self.publish(Some(block.slot), Durability::Buffer)?;
        self.indexer.send(IndexMessage::Block { slot: block.slot, span })?;
        if block.slot.is_multiple_of(SIZE_CHECK_FREQUENCY) && self.ledger.size_exceeded()? {
            self.sync()?;
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
        self.sync()?;
        self.writer.finalize()?;
        info!(superblock = seal.id, "sealed superblock");
        Ok(())
    }

    /// Writes and publishes a volatile-state reset marker.
    fn write_reset(&mut self, slot: Slot) -> Result<()> {
        self.writer.write_blockstore(&BlockstoreEntry::Reset(slot))?;
        self.sync()?;
        info!(slot, "appended volatile state reset");
        Ok(())
    }

    /// Makes files and preceding block indexes durable, publishes accumulated
    /// metadata, and broadcasts the new blockstore position.
    fn sync(&mut self) -> Result<()> {
        self.publish(None, Durability::SyncData)?;
        self.indexer.fence(None)
    }

    /// Publishes one data-file boundary with buffered or data-synced durability.
    fn publish(&mut self, slot: Option<Slot>, durability: Durability) -> Result<()> {
        let cursors = self.writer.persist(durability)?;
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

fn acknowledge(result: &Result<()>, response: Response<()>) {
    if result.is_ok() {
        let _ = response.send(());
    }
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
