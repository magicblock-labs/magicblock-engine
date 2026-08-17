//! Transaction execution coordination.

use std::{
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
};

use keeper::{ExecutionRecord, FullTransaction, Keeper, ResolvedTransaction};
use nucleus::{
    ledger::Block,
    shutdown::{Service, ShutdownHandle, ShutdownManager, ShutdownReason},
    tls::AUTHORITY,
};
use solana_program_runtime::loaded_programs::ProgramCache;
use solana_svm::transaction_processing_result::TransactionProcessingResultExtensions;
use tokio::sync::mpsc::Sender;
use tracing::{error, info};

use crate::{
    Result,
    callback::SVMCallback,
    metrics::{self, FailureKind},
    sequencer::{ReadyTransaction, Ticket},
    svm::SvmContext,
};

/// Index identifying an executor within the pool.
pub(crate) type ExecutorId = u32;

/// Work delivered from the sequencer to a transaction executor.
///
/// Keeping transactions inline avoids a heap allocation on every dispatch.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ExecutorMessage {
    /// One dependency-free transaction and its stable sequencer ticket.
    Transaction(ReadyTransaction),
    /// A block boundary advancing the executor's processing environment.
    Block(Block),
}

/// Transaction outcome reported from an executor to the sequencer.
pub(crate) enum ExecutorEvent {
    /// The transaction committed and its executor is available again.
    Completed { id: ExecutorId, ticket: Ticket },
    /// Executor infrastructure failed before the ticket could complete.
    Failed { id: ExecutorId },
}

/// Worker that drives the SVM for dependency-free transactions.
pub(crate) struct TransactionExecutor {
    /// This executor's index within the pool.
    id: ExecutorId,
    /// Inbound channel of transactions and block boundaries.
    rx: Receiver<ExecutorMessage>,
    /// Channel used to report transaction completion or executor failure.
    events: Sender<ExecutorEvent>,
    /// SVM batch processor and per-block environment driving execution.
    svm: SvmContext,
    /// Durable engine state used for account loads and commits.
    state: Arc<Keeper>,
    /// Handle used to report cooperative shutdown for this worker.
    shutdown: ShutdownHandle,
    /// Whether the executor runs in ledger-replay mode: when set, only state
    /// transitions are committed directly instead of recording full execution.
    replay: bool,
}

/// Sequencer-side handle to a spawned executor.
pub(crate) struct ExecutorHandle {
    /// Index of the executor this handle controls.
    id: ExecutorId,
    /// Channel for dispatching transactions and block boundaries to the worker.
    tx: SyncSender<ExecutorMessage>,
    /// Join handle for the worker thread, taken during shutdown.
    task: Option<JoinHandle<()>>,
}

impl ExecutorHandle {
    /// Sends work to this executor, preserving its service identity on failure.
    pub(crate) fn send(&self, message: ExecutorMessage) -> Result<()> {
        self.tx.send(message).map_err(|_| Service::TransactionExecutor(self.id).into())
    }

    /// Closes the dispatch channel and joins the executor thread, if spawned.
    pub(crate) fn join(mut self) {
        let task = self.task.take();
        drop(self);
        if let Some(task) = task {
            let _ = task.join();
        }
    }

    /// Builds an unspawned executor handle whose messages can be inspected.
    #[cfg(test)]
    pub(crate) fn mock(id: ExecutorId) -> (Self, Receiver<ExecutorMessage>) {
        let (tx, rx) = mpsc::sync_channel(1);
        (Self { id, tx, task: None }, rx)
    }
}

impl TransactionExecutor {
    /// Builds the SVM environment, spawns the worker thread, and returns its
    /// sequencer-side handle.
    pub(crate) fn spawn(
        id: ExecutorId,
        state: Arc<Keeper>,
        cache: Arc<ProgramCache>,
        shutdown: &mut ShutdownManager,
        events: Sender<ExecutorEvent>,
        replay: bool,
    ) -> Result<ExecutorHandle> {
        let svm = SvmContext::new(&state, cache)?;
        let shutdown = shutdown.handle(Service::TransactionExecutor(id));
        let (tx, rx) = mpsc::sync_channel(3);
        let executor = Self {
            id,
            rx,
            svm,
            events,
            state,
            shutdown,
            replay,
        };
        let task = thread::Builder::new()
            .name(format!("transaction-executor-{id}"))
            .spawn(move || executor.run())?;
        Ok(ExecutorHandle { id, task: Some(task), tx })
    }

    /// Worker loop: executes transactions and applies block transitions until
    /// the channel closes or execution fails, then reports the termination reason.
    fn run(mut self) {
        // MagicRoot authorizes callers against this thread-local; publish the
        // engine authority before executing any transaction on this thread.
        AUTHORITY.set(self.state.authority());
        let error = loop {
            let Ok(msg) = self.rx.recv() else {
                break None;
            };
            match msg {
                ExecutorMessage::Transaction(txn) => {
                    if let Err(error) = self.process(txn.transaction) {
                        drop(self.rx);
                        let event = ExecutorEvent::Failed { id: self.id };
                        let _ = self.events.blocking_send(event);
                        break Some(error);
                    }
                    let event = ExecutorEvent::Completed { id: self.id, ticket: txn.ticket };
                    if self.events.blocking_send(event).is_err() {
                        info!(id = self.id, "event channel closed, executor exiting");
                        break None;
                    }
                }
                ExecutorMessage::Block(block) => self.svm.transition(block),
            };
        };
        let reason = if let Some(error) = error {
            error!(?error, self.id, "executor failed, terminating");
            ShutdownReason::Error(Box::new(error))
        } else {
            ShutdownReason::Signalled
        };
        self.shutdown.terminate(reason);
    }

    /// Loads and executes one transaction through the SVM, committing either
    /// its raw state transition (replay) or full execution.
    fn process(&mut self, txn: ResolvedTransaction) -> Result<()> {
        let accessor = self.state.accounts();
        let callback = SVMCallback::<false> {
            loader: accessor.loader(),
            featureset: self.state.features(),
        };
        let output = self.svm.execute(&callback, &txn, self.state.features());
        if !output.processing_result.was_processed_with_successful_result() {
            metrics::failed_transaction(FailureKind::Execution);
        }
        if self.replay {
            self.state.transactions().commit_state_transitions(&output.processing_result)?;
        } else {
            let txn = FullTransaction {
                transaction: txn.into_view(),
                execution: ExecutionRecord {
                    result: output.processing_result,
                    balances: output.balance_collector,
                    slot: self.svm.slot(),
                },
            };
            self.state.transactions().commit_execution(txn)?;
        }
        Ok(())
    }
}
