//! Runtime-facing shared types: transaction views, execution I/O, scheduling
//! messages, and the schema of the engine's built-in MagicRoot program (its
//! address, instruction set, and authority). The MagicRoot execution logic
//! lives in the `magic-root-program` crate, which depends on these shared
//! definitions.

use std::sync::Arc;

use agave_transaction_view::{
    resolved_transaction_view::ResolvedTransactionView, transaction_view::SanitizedTransactionView,
};
use derive_more::{Deref, From};
use solana_svm::{
    transaction_balances::BalanceCollector,
    transaction_processing_result::TransactionProcessingResult,
};
use solana_transaction_error::TransactionResult;
use tokio::sync::mpsc::Sender;

use crate::{Slot, ledger::Block};

/// Sanitized transaction view backed by a shared, immutable payload buffer.
pub type TransactionView = SanitizedTransactionView<Arc<Vec<u8>>>;
/// Sanitized transaction view with its account keys already resolved.
pub type ResolvedTransaction = ResolvedTransactionView<Arc<Vec<u8>>>;
/// Dropping this handle releases a sequencer quiescence barrier.
pub type BarrierHandle = oneshot::Sender<()>;

/// Cloneable submission handle into the sequencer's execution and simulation
/// channels.
#[derive(Clone, Deref)]
pub struct SequencerHandle {
    /// Channel for submitting transactions to be executed and committed.
    #[deref]
    pub execution: Sender<SequencerMessage>,
    /// Channel for submitting transactions to be simulated without committing.
    pub simulation: Sender<SimulatorMessage>,
}

/// Work item handed to the transaction sequencer.
#[derive(From)]
pub enum SequencerMessage {
    /// A transaction to schedule and execute.
    Transaction(TransactionView),
    /// A block boundary to seal before scheduling further transactions.
    Block(Block),
    /// Finalize a block boundary, then pause before accepting subsequent work.
    Checkpoint(Block, BarrierGuard),
    /// Quiesce the sequencer and all its executors until released — used to take
    /// a consistent snapshot at superblock boundaries (see ledger replay and
    /// `finalize_superblock`).
    Barrier(BarrierGuard),
}

/// Quiescence barrier handed to a running service.
///
/// On receipt the service drains its in-flight work, signals `acknowledged`,
/// then blocks on `released` before resuming. Paired with a [`BarrierController`]
/// via [`barrier`].
pub struct BarrierGuard {
    /// Signals the controller that the service is now idle.
    pub acknowledged: BarrierHandle,
    /// Resolves when the controller permits the service to resume; a dropped
    /// controller also releases it.
    pub released: oneshot::Receiver<()>,
}

/// Controller side of a quiescence barrier, held by the caller that raised it.
///
/// Awaits `acknowledged` to learn the service is quiesced, then sends `released`
/// (or drops) to let it resume.
pub struct BarrierController {
    /// Resolves once the service reports it has gone idle.
    pub acknowledged: oneshot::Receiver<()>,
    /// Releases the service to resume operation.
    pub released: BarrierHandle,
}

/// Constructs a paired [`BarrierController`] and [`BarrierGuard`] over two
/// oneshot channels.
pub fn barrier() -> (BarrierController, BarrierGuard) {
    let (acknowledged_tx, acknowledged_rx) = oneshot::channel();
    let (released_tx, released_rx) = oneshot::channel();
    let controller = BarrierController {
        acknowledged: acknowledged_rx,
        released: released_tx,
    };
    let guard = BarrierGuard {
        acknowledged: acknowledged_tx,
        released: released_rx,
    };
    (controller, guard)
}

/// Work item handed to the transaction simulator.
#[derive(From)]
pub enum SimulatorMessage {
    /// A transaction to simulate against the current state.
    Transaction(Simulation),
    /// A block boundary advancing the simulator's environment.
    Block(Block),
    /// Quiesce the simulator until released — keeps it idle while a consistent
    /// snapshot is taken at superblock boundaries.
    Barrier(BarrierGuard),
}

/// A single simulation request and the channel to deliver its outcome.
pub struct Simulation {
    /// The transaction to simulate.
    pub transaction: TransactionView,
    /// Where the simulation result is returned to the caller.
    pub response: oneshot::Sender<TransactionResult<ExecutionRecord>>,
}

/// Transaction payload paired with the SVM output needed to finalize state.
pub struct FullTransaction {
    /// Sanitized transaction bytes and signatures accepted by the ledger.
    pub transaction: TransactionView,
    /// SVM execution output for the transaction.
    pub execution: ExecutionRecord,
}

/// SVM output produced by executing a transaction.
pub struct ExecutionRecord {
    /// SVM processing result produced for this transaction.
    pub result: TransactionProcessingResult,
    /// Native pre/post balances collected during execution.
    pub balances: Option<BalanceCollector>,
    /// Slot assigned to the execution result.
    pub slot: Slot,
}
