//! Transaction sequencer: preserves input order for account conflicts and fans
//! dependency-free transactions out to a pool of executors.

use std::{sync::Arc, thread};

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
use tracing::{debug, error, info};

use crate::{
    ProcessorError, Result, SequencerMessage,
    executor::{ExecutorEvent, TransactionExecutor},
    metrics::{self, FailureKind, Operation},
    simulator::TransactionSimulator,
};

use self::{
    order::OrderingTable,
    pool::{Executors, MAX_EXECUTORS},
};

mod order;
mod pool;

#[cfg(test)]
mod tests;

/// Maximum pending transactions per executor before applying backpressure.
const MAX_PENDING_EXECUTOR_TXNS: usize = 16;

/// Stable index of a transaction node within one drain epoch.
pub(crate) type Ticket = usize;

/// Dependency-free transaction handed from the ordering DAG to an executor.
pub(crate) struct ReadyTransaction {
    /// Stable node index returned by the executor on completion.
    pub(crate) ticket: Ticket,
    /// Transaction whose predecessor count reached zero.
    pub(crate) transaction: ResolvedTransaction,
}

/// Schedules inbound transactions onto executors, ordering per-account
/// dependencies and finalizing block boundaries.
pub struct Sequencer {
    /// Durable engine state used for appends and account/block lookups.
    state: Arc<Keeper>,
    /// Block-local dependency graph preserving input order for account conflicts.
    ordering: OrderingTable,
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
        let count = executors.clamp(1, MAX_EXECUTORS as usize);
        metrics::init();
        let (event_tx, event_rx) = mpsc::channel(count);
        let mut executors = Vec::with_capacity(count);
        for id in 0..count as u32 {
            let handle = TransactionExecutor::spawn(
                id,
                state.clone(),
                cache.clone(),
                shutdown,
                event_tx.clone(),
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
            ordering: Default::default(),
            executors: Executors::new(executors, event_rx),
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

    /// Main loop: handles executor completions, inbound messages, and shutdown,
    /// then drains on orderly cancellation and joins the executor threads.
    async fn run(mut self) {
        let mut reason = loop {
            let result = tokio::select! {
                biased;
                Some(event) = self.executors.events.recv() => {
                    self.handle_event(event)
                }
                _ = self.shutdown.signalled() => {
                    break ShutdownReason::Signalled;
                }
                Some(msg) = self.rx.recv(), if self.accepting() => {
                    self.handle_message(msg).await
                }
            };
            if let Err(error) = result {
                error!(?error, "sequencer failed, terminating");
                break ShutdownReason::Error(Box::new(error));
            }
        };
        if matches!(reason, ShutdownReason::Signalled) {
            info!("sequencer is draining the in-flight work");
            if let Err(error) = self.drain().await {
                error!(?error, "sequencer drain failed, terminating");
                reason = ShutdownReason::Error(Box::new(error));
            }
        }
        self.executors.join();
        // Release engine storage before the manager can reopen it.
        drop(self.state);
        self.shutdown.terminate(reason);
    }

    /// Dispatches an inbound message to the transaction or block handler.
    async fn handle_message(&mut self, msg: SequencerMessage) -> Result<()> {
        match msg {
            SequencerMessage::Transaction(txn) => self.schedule(txn).await,
            SequencerMessage::Block(block) => self.finalize(block).await,
            SequencerMessage::Checkpoint(block, guard) => {
                self.finalize(block).await?;
                self.pause(guard).await;
                Ok(())
            }
            SequencerMessage::Barrier(guard) => self.barrier(guard).await,
        }
    }

    /// Registers a transaction in input order and dispatches all ready work.
    async fn schedule(&mut self, txn: TransactionView) -> Result<()> {
        let Ok(txn) = ResolvedTransaction::try_new(txn, None, &Default::default()) else {
            metrics::failed_transaction(FailureKind::SequencerDrop);
            return Ok(());
        };
        if !self.replay {
            if !self.state.transactions().append(&txn).await? {
                metrics::failed_transaction(FailureKind::SequencerDrop);
                return Ok(());
            }
            self.hasher.update(&txn.signatures()[0]);
        }
        if self.ordering.register(txn) {
            metrics::ordering_dependency();
            metrics::blocked_transaction();
        }
        self.dispatch_ready()
    }

    /// Applies an executor event or fail-stops without releasing failed work.
    fn handle_event(&mut self, event: ExecutorEvent) -> Result<()> {
        let (id, ticket) = match event {
            ExecutorEvent::Completed { id, ticket } => (id, ticket),
            ExecutorEvent::Failed { id } => {
                return Err(Service::TransactionExecutor(id).into());
            }
        };

        self.executors.release(id);
        let released = self.ordering.complete(ticket);
        if released != 0 {
            metrics::unblocked_transactions(released);
        }
        self.dispatch_ready()
    }

    /// Whether another inbound message fits within the pending-work bound.
    fn accepting(&self) -> bool {
        self.ordering.len() < MAX_PENDING_EXECUTOR_TXNS * self.executors.capacity()
    }

    /// Fills all available executors from the dependency-ready queue.
    fn dispatch_ready(&mut self) -> Result<()> {
        while let Some(id) = self.executors.available() {
            let Some(transaction) = self.ordering.take_ready() else {
                break;
            };
            self.executors.dispatch(id, transaction)?;
        }
        Ok(())
    }

    /// Raises a quiescence barrier: drains all in-flight work so the sequencer
    /// and its executors are idle, acknowledges the controller, then waits to be
    /// released before resuming. Used to take a consistent state snapshot at
    /// superblock boundaries.
    async fn barrier(&mut self, guard: BarrierGuard) -> Result<()> {
        self.drain().await?;
        self.pause(guard).await;
        Ok(())
    }

    /// Acknowledges quiescence and holds the sequencer until released.
    async fn pause(&self, guard: BarrierGuard) {
        info!("sequencer is halting operation");
        let _ = guard.acknowledged.send(());
        let _ = guard.released.await;
        info!("sequencer is resuming operation");
    }

    /// Awaits executor-ready signals, reclaiming each finished executor
    /// until the whole pool is idle. Used to ensure that the in-flight
    /// work is complete before finalizing a block and during shutdown.
    async fn drain(&mut self) -> Result<()> {
        let _timer = metrics::time(Operation::BarrierDrain);
        self.dispatch_ready()?;
        while !self.ordering.is_empty() {
            let Some(event) = self.executors.events.recv().await else {
                debug!("executor pool closed during drain");
                return Err(ProcessorError::Internal(
                    "executor pool closed with pending dependencies".into(),
                ));
            };
            self.handle_event(event)?;
        }
        self.ordering.reset();
        Ok(())
    }

    /// Finalizes the current block: chains its hash, appends it, and notifies
    /// every executor of the new block boundary.
    async fn finalize(&mut self, mut block: Block) -> Result<()> {
        let _timer = metrics::time(Operation::FinalizeBlock);
        if self.replay && block.parent != self.hasher.parent {
            return Err(ProcessorError::Internal(format!(
                "replayed block {} has parent {:?}, expected {:?}",
                block.slot, block.parent, self.hasher.parent
            )));
        }
        if !self.replay {
            block.parent = self.hasher.parent;
            block.hash = self.hasher.finalize();
        }

        // Block boundaries synchronize executors:
        // 1. sysvar writes bypass declared account dependencies and must be ordered
        // 2. replaying should schedule transactions in their original block
        self.drain().await?;
        self.executors.transition(block)?;
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
