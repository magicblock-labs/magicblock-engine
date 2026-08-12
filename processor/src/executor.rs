//! Transaction execution coordination.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
};

use ahash::HashMap;
use derive_more::{Deref, DerefMut};
use keeper::{ExecutionRecord, FullTransaction, Keeper};
use nucleus::{
    shutdown::{Service, ShutdownHandle, ShutdownManager, ShutdownReason},
    tls::AUTHORITY,
};
use solana_program_runtime::loaded_programs::ProgramCache;
use solana_pubkey::Pubkey;
use solana_svm::transaction_processing_result::TransactionProcessingResultExtensions;
use tokio::sync::mpsc::Sender;
use tracing::{error, info};

use crate::{
    ExecutorMessage, ExecutorReady, ResolvedTransaction, Result,
    callback::SVMCallback,
    metrics::{self, FailureKind},
    svm::SvmContext,
};

/// Index identifying an executor within the pool.
pub(crate) type ExecutorId = u32;

/// Sequencer-owned work accumulated for an executor between dispatches.
#[derive(Default)]
pub(crate) struct ExecutorWork {
    /// Transactions accumulated for the next dispatch.
    pub(crate) batch: Vec<ResolvedTransaction>,
    /// Accounts held on this executor's behalf, mapped to their reference count.
    pub(crate) locks: HashMap<Pubkey, usize>,
    /// Transactions queued behind this executor on lock contention.
    pub(crate) blocked: VecDeque<ResolvedTransaction>,
}

/// Worker that drives the SVM for batches of conflict-free transactions.
pub(crate) struct TransactionExecutor {
    /// This executor's index within the pool.
    id: ExecutorId,
    /// Inbound channel of execution batches and block boundaries.
    rx: Receiver<ExecutorMessage>,
    /// Channel used to report back to the sequencer once a batch completes.
    ready: Sender<ExecutorReady>,
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

/// Sequencer-side handle to a spawned executor: its dispatch channel, pending
/// batch, held locks, and join handle.
#[derive(Deref, DerefMut)]
pub(crate) struct ExecutorHandle {
    /// Index of the executor this handle controls.
    pub(crate) id: ExecutorId,
    /// Mutable sequencing state moved out while completed work is reclaimed.
    #[deref]
    #[deref_mut]
    pub(crate) work: ExecutorWork,
    /// Channel for dispatching batches and block boundaries to the worker.
    pub(crate) tx: SyncSender<ExecutorMessage>,
    /// Join handle for the worker thread, taken during shutdown.
    pub(crate) task: Option<JoinHandle<()>>,
}

impl TransactionExecutor {
    /// Builds the SVM environment, spawns the worker thread, and returns its
    /// sequencer-side handle.
    pub(crate) fn spawn(
        id: ExecutorId,
        state: Arc<Keeper>,
        cache: Arc<ProgramCache>,
        shutdown: &mut ShutdownManager,
        ready: Sender<ExecutorReady>,
        replay: bool,
    ) -> Result<ExecutorHandle> {
        let svm = SvmContext::new(&state, cache)?;
        let shutdown = shutdown.handle(Service::TransactionExecutor(id));
        let (tx, rx) = mpsc::sync_channel(3);
        let executor = Self {
            id,
            rx,
            svm,
            ready,
            state,
            shutdown,
            replay,
        };
        let task = thread::Builder::new()
            .name(format!("transaction-executor-{id}"))
            .spawn(move || executor.run())?;
        Ok(ExecutorHandle {
            id,
            task: Some(task),
            tx,
            work: Default::default(),
        })
    }

    /// Worker loop: executes batches and applies block transitions until the
    /// channel closes or a batch fails, then reports the termination reason.
    fn run(mut self) {
        // MagicRoot authorizes callers against this thread-local; publish the
        // engine authority before executing any transaction on this thread.
        AUTHORITY.set(self.state.authority());
        let mut error = None;
        while let Ok(msg) = self.rx.recv() {
            match msg {
                ExecutorMessage::Transactions(mut batch) => {
                    let result = self.process(&mut batch);
                    if let Err(e) = result {
                        error.replace(e);
                        drop(self.rx);
                        let signal = ExecutorReady { id: self.id, batch };
                        let _ = self.ready.blocking_send(signal);
                        break;
                    }
                    let signal = ExecutorReady { id: self.id, batch };
                    if self.ready.blocking_send(signal).is_err() {
                        info!(id = self.id, "ready channel closed, executor exiting");
                        break;
                    }
                }
                ExecutorMessage::Block(block) => self.svm.transition(block),
            };
        }
        let reason = if let Some(error) = error {
            error!(?error, self.id, "executor failed, terminating");
            ShutdownReason::Error(Box::new(error))
        } else {
            ShutdownReason::Signalled
        };
        self.shutdown.terminate(reason);
    }

    /// Loads and executes each transaction in the batch through the SVM,
    /// committing either raw state transitions (replay) or full execution.
    fn process(&mut self, transactions: &mut Vec<ResolvedTransaction>) -> Result<()> {
        let accessor = self.state.accounts();
        let callback = SVMCallback::<false> {
            loader: accessor.loader(),
            featureset: self.state.features(),
        };
        for txn in transactions.drain(..) {
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
        }
        Ok(())
    }
}
