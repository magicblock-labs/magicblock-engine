//! Transaction sequencer: resolves account-lock conflicts and fans
//! non-conflicting transactions out to a pool of executors.

use std::{mem, sync::Arc, thread};

use blake3::Hasher;
use keeper::{Keeper, ResolvedTransaction, TransactionView};
use nucleus::{
    Slot,
    ledger::Block,
    runtime::{BarrierGuard, SequencerHandle},
    shutdown::{Service, ShutdownHandle, ShutdownManager, ShutdownReason},
};
use solana_hash::Hash;
use solana_program_runtime::loaded_programs::ProgramCache;
use solana_signature::Signature;
use tokio::{runtime::Builder, sync::mpsc};
use tracing::{debug, error, info, warn};

mod locks;
mod pool;

#[cfg(test)]
mod tests;

use self::{
    locks::{LockTable, MAX_EXECUTORS},
    pool::Executors,
};
use crate::{
    ExecutorMessage, ExecutorReady, Result, SequencerMessage,
    executor::TransactionExecutor,
    metrics::{self, FailureKind, Operation},
    simulator::TransactionSimulator,
};

/// Per-executor blocked-queue threshold that pauses inbound scheduling.
/// Retry requeueing may temporarily grow a queue beyond this threshold.
const MAX_BLOCKED_EXECUTOR_TXNS: usize = 16;

/// Schedules inbound transactions onto executors, ordering them by per-account
/// lock conflicts and finalizing block boundaries.
pub struct Sequencer {
    /// Durable engine state used for appends and account/block lookups.
    state: Arc<Keeper>,
    /// Per-account locks held by in-flight transactions, used to detect conflicts.
    locks: LockTable,
    /// Inbound stream of transactions and block boundaries.
    rx: mpsc::Receiver<SequencerMessage>,
    /// Hash-chain state for the block currently being sequenced.
    hasher: BlockHasher,
    /// The executor pool and its availability bookkeeping.
    executors: Executors,
    /// Slot currently being sequenced.
    slot: Slot,
    /// Handle used to observe and report cooperative shutdown.
    shutdown: ShutdownHandle,
    /// Whether the sequencer runs in ledger-replay mode; propagated to every
    /// spawned executor (see the executor's `replay` field).
    replay: bool,
}

impl Sequencer {
    /// Spawns the executor pool and assembles a sequencer over the given state.
    pub fn new(
        executors: usize,
        state: Arc<Keeper>,
        cache: Arc<ProgramCache>,
        shutdown: &mut ShutdownManager,
        replay: bool,
    ) -> Result<(Self, SequencerHandle)> {
        let count = executors.min(MAX_EXECUTORS as usize);
        metrics::init();
        let (ready_tx, ready_rx) = mpsc::channel(count);
        let mut executors = Vec::with_capacity(count);
        for id in 0..count as u32 {
            let handle = TransactionExecutor::spawn(
                id,
                state.clone(),
                cache.clone(),
                shutdown,
                ready_tx.clone(),
                replay,
            )?;
            executors.push(handle);
        }

        let hasher = BlockHasher::new(state.blockhash());

        let (execution, rx) = mpsc::channel(1024);
        let simulation = TransactionSimulator::spawn(state.clone(), cache, shutdown)?;
        let shutdown = shutdown.handle(Service::Sequencer);

        let sequencer = Self {
            slot: state.blocks().current_slot(),
            state,
            locks: Default::default(),
            executors: Executors::new(executors, ready_rx),
            rx,
            hasher,
            shutdown,
            replay,
        };
        let handle = SequencerHandle { execution, simulation };
        info!(executors = count, replay, "sequencer started");
        Ok((sequencer, handle))
    }

    /// Moves the sequencer onto its own thread with a current-thread runtime.
    pub fn spawn(self) -> Result<()> {
        let runtime = Builder::new_current_thread().build()?;
        thread::Builder::new()
            .name("transaction-sequencer".into())
            .spawn(move || runtime.block_on(self.run()))?;
        Ok(())
    }

    /// Main loop: drains executor-ready signals, inbound messages, and the
    /// shutdown signal until cancellation, then joins the executor threads.
    async fn run(mut self) {
        loop {
            let result = tokio::select! {
                biased;
                Some(msg) = self.executors.ready.recv() => {
                    self.handle_ready(msg)
                }
                _ = self.shutdown.signalled() => {
                    break;
                }
                Some(msg) = self.rx.recv(), if self.executors.ready() => {
                    self.handle_message(msg).await
                }
            };
            if let Err(error) = result {
                error!(?error, "sequencer failed, terminating");
                break;
            }
        }
        info!("sequencer shutdown is requested, draining the in-flight work");
        let _ = self.drain().await;
        for mut e in self.executors.handles.drain(..) {
            let handle = e.task.take();
            drop(e);
            if let Some(handle) = handle {
                let _ = handle.join();
            }
        }
        // Release engine storage before the manager can reopen it.
        drop(self.state);
        self.shutdown.terminate(ShutdownReason::Signalled);
    }

    /// Dispatches an inbound message to the transaction or block handler.
    async fn handle_message(&mut self, msg: SequencerMessage) -> Result<()> {
        match msg {
            SequencerMessage::Transaction(txn) => self.schedule(txn).await,
            SequencerMessage::Block(block) => self.finalize(block).await,
            SequencerMessage::Barrier(guard) => self.barrier(guard).await,
        }
    }

    /// Resolves account-lock conflicts for a transaction and either dispatches
    /// it to a free executor or queues it behind the executor that blocks it.
    async fn schedule(&mut self, txn: TransactionView) -> Result<()> {
        let Ok(txn) = ResolvedTransaction::try_new(txn, None, &Default::default()) else {
            metrics::failed_transaction(FailureKind::SequencerDrop);
            return Ok(());
        };
        if !self.replay {
            if !self.state.transactions().append(&txn).await? {
                // Dropping a duplicate is right — it must not execute twice —
                // but dropping it *silently* strands whoever submitted it,
                // because waiters are keyed by signature and no further status
                // will ever be published for one that has already settled. Hand
                // them the original's result instead.
                self.state.transactions().notify_duplicate(txn.signatures()[0]);
                metrics::failed_transaction(FailureKind::SequencerDrop);
                return Ok(());
            }
            self.hasher.update(&txn.signatures()[0]);
        }
        let Some(executor) = self.executors.available() else {
            // All executors are busy; park this unlocked transaction on executor 0.
            self.executors.enqueue(txn, 0);
            return Ok(());
        };
        if let Err(blocker) = self.locks.acquire(executor, &txn) {
            metrics::lock_conflict();
            self.executors.enqueue(txn, blocker);
            return Ok(());
        }
        executor.batch.push(txn);
        let id = executor.id;
        self.executors.dispatch(id)
    }

    /// Reclaims an executor that finished its batch: releases the account locks
    /// it held, then re-tries the transactions queued behind it. Any that can
    /// now acquire their locks form a fresh batch dispatched back to it; the
    /// rest are re-queued behind whichever executor still blocks them.
    fn handle_ready(&mut self, msg: ExecutorReady) -> Result<()> {
        let id = msg.id;
        let idx = id as usize;
        let Some(executor) = self.executors.release(id) else {
            warn!(id, "ready signal for unknown executor; ignoring");
            return Ok(());
        };
        executor.batch = msg.batch;
        self.locks.release(executor);
        let mut blocked = mem::take(&mut executor.blocked);
        // Lock acquisition is reentrant per executor, so a retry cannot be
        // enqueued back into its own detached queue.
        while let Some(txn) = blocked.pop_front() {
            metrics::unblocked_transaction();
            let executor = &mut self.executors.handles[idx];
            if let Err(blocker) = self.locks.acquire(executor, &txn) {
                metrics::lock_conflict();
                self.executors.enqueue(txn, blocker);
                continue;
            }
            executor.batch.push(txn);
        }
        self.executors.handles[idx].blocked = blocked;
        self.executors.dispatch(id)
    }

    /// Raises a quiescence barrier: drains all in-flight work so the sequencer
    /// and its executors are idle, acknowledges the controller, then waits to be
    /// released before resuming. Used to take a consistent state snapshot at
    /// superblock boundaries.
    async fn barrier(&mut self, guard: BarrierGuard) -> Result<()> {
        info!("sequencer is halting operation");
        self.drain().await?;
        let _ = guard.acknowledged.send(());
        let _ = guard.released.await;
        info!("sequencer is resuming operation");
        Ok(())
    }

    /// Awaits executor-ready signals, reclaiming each finished executor
    /// until the whole pool is idle. Used to ensure that the in-flight
    /// work is complete before finalizing a block and during shutdown.
    async fn drain(&mut self) -> Result<()> {
        let _timer = metrics::time(Operation::BarrierDrain);
        while !self.executors.idle() {
            let Some(signal) = self.executors.ready.recv().await else {
                debug!("executor pool closed during drain; abandoning in-flight work");
                return Ok(());
            };
            self.handle_ready(signal)?;
        }
        Ok(())
    }

    /// Finalizes the current block: chains its hash, appends it, and notifies
    /// every executor of the new block boundary.
    async fn finalize(&mut self, mut block: Block) -> Result<()> {
        let _timer = metrics::time(Operation::FinalizeBlock);
        block.parent = self.hasher.parent;
        block.hash = self.hasher.finalize();

        // Block boundaries synchronize executors:
        // 1. sysvar writes bypass account locks and must be ordered deterministically
        // 2. replaying should schedule transactions in their original block
        self.drain().await?;
        for e in &self.executors.handles {
            e.tx.send(ExecutorMessage::Block(block))
                .map_err(|_| Service::TransactionExecutor(e.id))?;
        }
        self.state.blocks().append(block, self.replay)?;
        self.hasher.advance(block.hash);
        self.slot = self.state.blocks().current_slot();
        Ok(())
    }
}

/// Rolling state for the locally computed block-hash chain.
struct BlockHasher {
    /// Hasher seeded with `parent` for the block currently being finalized.
    current: Hasher,
    /// Latest committed block hash.
    parent: Hash,
}

impl BlockHasher {
    /// Starts a block hash from the latest committed block hash.
    fn new(parent: Hash) -> Self {
        let mut current = Hasher::new();
        current.update(parent.as_ref());
        Self { current, parent }
    }

    /// Adds an appended transaction's canonical signature to the current block.
    fn update(&mut self, signature: &Signature) {
        self.current.update(signature.as_ref());
    }

    /// Finalizes the current block hash without advancing committed state.
    fn finalize(&self) -> Hash {
        Hash::from(*self.current.finalize().as_bytes())
    }

    /// Advances the chain after the finalized block has committed successfully.
    fn advance(&mut self, hash: Hash) {
        self.parent = hash;
        self.current.reset();
        self.current.update(hash.as_ref());
    }
}
