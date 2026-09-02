//! Ordered background ledger index service.

use std::sync::Arc;

use fjall::Keyspace;
use flume::{Receiver, Sender};
use nucleus::Slot;
use solana_signature::Signature;

use crate::{
    Ledger,
    error::{LedgerError, Result},
    index::{IndexWriter, Span, TxSpan},
    schema::AccountIndex,
    storage::Durability,
};

/// Ordered work sent by the appender after it has assigned file spans.
pub(crate) enum IndexMessage {
    /// Transaction and account entries held until the following block marker.
    Transaction {
        signature: Signature,
        accounts: AccountIndex,
        span: TxSpan,
    },
    /// Commits all preceding transaction entries atomically with this block.
    Block { slot: Slot, span: Span },
    /// Makes preceding blocks durable or starts a successor keyspace.
    Fence {
        next: Option<Keyspace>,
        response: oneshot::Sender<()>,
    },
}

/// Sole appender-side owner of the bounded index queue.
pub(crate) struct IndexerHandle(Sender<IndexMessage>);

impl IndexerHandle {
    pub(crate) fn new(tx: Sender<IndexMessage>) -> Self {
        Self(tx)
    }

    /// Enqueues ordered index work, reporting a failed worker precisely.
    pub(crate) fn send(&self, message: IndexMessage) -> Result<()> {
        self.0.send(message).map_err(|_| LedgerError::IndexerClosed)
    }

    /// Fences preceding blocks, syncing in place or rotating to `next`.
    pub(crate) fn fence(&self, next: Option<Keyspace>) -> Result<()> {
        let (response, acknowledged) = oneshot::channel();
        self.send(IndexMessage::Fence { next, response })?;
        acknowledged.recv().map_err(|_| LedgerError::IndexerFenceClosed)
    }
}

/// Single ordered worker that owns all Fjall index mutation.
pub(crate) struct LedgerIndexer {
    /// Shared ledger state used to open keyspaces for rotation.
    ledger: Arc<Ledger>,
    /// Fjall batch writer for the active superblock.
    writer: IndexWriter,
    /// Ordered work stream owned exclusively by this worker.
    rx: Receiver<IndexMessage>,
}

impl LedgerIndexer {
    pub(crate) fn new(ledger: Arc<Ledger>, rx: Receiver<IndexMessage>) -> Result<Self> {
        let head = ledger.meta.head();
        let keyspace = ledger
            .superblocks
            .read()
            .get(&head)
            .map(|superblock| superblock.index.clone())
            .ok_or(LedgerError::Corruption("active superblock missing"))?;
        let writer = ledger.index.writer(&keyspace);
        Ok(Self { ledger, writer, rx })
    }

    /// Drains ordered work until the appender closes the sole sender.
    pub(crate) fn run(mut self) -> Result<()> {
        while let Ok(message) = self.rx.recv() {
            match message {
                IndexMessage::Transaction { signature, accounts, span } => {
                    self.writer.insert_transaction(&signature, span);
                    self.writer.insert_accounts(&accounts, span.execution);
                }
                IndexMessage::Block { slot, span } => self.commit_block(slot, span)?,
                IndexMessage::Fence { next, response } => {
                    if let Some(keyspace) = next {
                        self.writer.rotate_memtable()?;
                        self.writer = self.ledger.index.writer(&keyspace);
                    } else {
                        self.writer.sync()?;
                    }
                    let _ = response.send(());
                }
            }
        }
        Ok(())
    }

    /// Atomically publishes one block's transaction, account, and block entries.
    fn commit_block(&mut self, slot: Slot, block: Span) -> Result<()> {
        self.writer.insert_block(slot, block);
        self.writer.persist(Durability::Buffer)
    }
}
