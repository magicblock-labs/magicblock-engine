//! Transaction simulation: executes transactions against current state on
//! owned account copies, without committing any changes.

use std::{sync::Arc, thread};

use keeper::{ExecutionRecord, Keeper, ResolvedTransaction};
use nucleus::{
    shutdown::{Service, ShutdownHandle, ShutdownManager, ShutdownReason},
    tls::AUTHORITY,
};
use solana_program_runtime::loaded_programs::ProgramCache;
use solana_transaction_error::TransactionError;
use tokio::{
    runtime::Builder,
    sync::mpsc::{self, Receiver, Sender},
};
use tracing::debug;

use crate::{Result, Simulation, SimulatorMessage, callback::SVMCallback, svm::SvmContext};

/// Worker that simulates transactions against current state without committing.
pub struct TransactionSimulator {
    /// Inbound channel of simulation requests and block boundaries.
    rx: Receiver<SimulatorMessage>,
    /// SVM batch processor and per-block environment driving simulation.
    svm: SvmContext,
    /// Durable engine state used for account loads.
    state: Arc<Keeper>,
    /// Handle used to report cooperative shutdown for this worker.
    shutdown: ShutdownHandle,
}

impl TransactionSimulator {
    /// Builds the SVM environment and spawns the simulator on its own thread.
    pub fn spawn(
        state: Arc<Keeper>,
        cache: Arc<ProgramCache>,
        shutdown: &mut ShutdownManager,
    ) -> Result<Sender<SimulatorMessage>> {
        let svm = SvmContext::new(&state, cache)?;
        let shutdown = shutdown.handle(Service::TransactionSimulator);
        let (tx, rx) = mpsc::channel(16);
        let executor = Self { rx, svm, state, shutdown };
        let runtime = Builder::new_current_thread().build()?;
        thread::Builder::new()
            .name("transaction-simulator".into())
            .spawn(move || runtime.block_on(executor.run()))?;
        Ok(tx)
    }

    /// Worker loop: simulates requests and applies block transitions until the
    /// channel closes, then reports cooperative shutdown.
    async fn run(mut self) {
        // Mirror the executor: simulated MagicRoot calls authorize against the
        // same authority published on this simulator thread.
        AUTHORITY.set(self.state.authority());
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.signalled() => {
                    break;
                }
                Some(msg) = self.rx.recv() => {
                    self.handle_message(msg);
                }
            }
        }
        // Release engine storage before the manager can reopen it.
        drop(self.state);
        self.shutdown.terminate(ShutdownReason::Signalled);
    }

    fn handle_message(&mut self, msg: SimulatorMessage) {
        match msg {
            SimulatorMessage::Transaction(simulation) => {
                self.process(simulation);
            }
            SimulatorMessage::Block(block) => self.svm.transition(block),
            SimulatorMessage::Barrier(guard) => {
                let _ = guard.acknowledged.send(());
                let _ = guard.released.recv();
            }
        };
    }

    /// Resolves and executes a single transaction on owned account copies,
    /// producing a result without persisting any state.
    fn process(&mut self, simulation: Simulation) {
        let result =
            ResolvedTransaction::try_new(simulation.transaction, None, &Default::default());
        let Ok(txn) = result else {
            debug!("simulation rejected: transaction resolution failed");
            let error = Err(TransactionError::InvalidAddressLookupTableData);
            let _ = simulation.response.send(error);
            return;
        };
        let accessor = self.state.accounts();
        let callback = SVMCallback::<true> {
            loader: accessor.loader(),
            featureset: self.state.features(),
        };
        let output = self.svm.execute(&callback, &txn, self.state.features());
        let execution = ExecutionRecord {
            result: output.processing_result,
            balances: output.balance_collector,
            slot: self.svm.slot(),
        };
        let _ = simulation.response.send(Ok(execution));
    }
}
