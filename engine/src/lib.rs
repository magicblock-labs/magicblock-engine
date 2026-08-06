#![doc = include_str!("../README.md")]

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use derive_more::Deref;
use keeper::{Keeper, builder::KeeperBuilder, error::KeeperError};
use ledger::schema::OwnedBlockstoreEntry;
use magic_root_program::entrypoint::MagicRootEntrypoint;
use nucleus::{
    runtime::{self, BarrierHandle, SequencerHandle},
    shutdown::{Service, ShutdownManager, ShutdownReason},
};
use processor::{SequencerMessage, sequencer::Sequencer};
use solana_program_runtime::{
    loaded_programs::{ProgramCache, ProgramCacheEntry},
    solana_sbpf::program::BuiltinFunctionDefinition,
};
use solana_pubkey::Pubkey;
use solana_system_program::system_processor::Entrypoint as SystemProgramEntrypoint;
use tracing::{error, info};

mod accessor;
mod error;
pub mod pacemaker;
mod transaction;

#[cfg(feature = "testkit")]
pub mod testkit;

pub use accessor::{AccountAccessor, TransactionAccessor};
pub use error::{EngineError, ReplayError, Result};
pub use transaction::IntoTransactionView;

use crate::pacemaker::{ExternalPacer, PaceMaker};

/// Top-level engine handle: owns the durable state and the sequencer submission
/// channels.
#[derive(Deref, Clone)]
pub struct Engine {
    /// Durable engine state (accountsdb + ledger), shared across components.
    #[deref]
    state: Arc<Keeper>,
    /// Submission handle into the sequencer's execution and simulation channels.
    sequencer: SequencerHandle,
    /// Rejects new transactions once coordinated shutdown begins.
    terminating: Arc<AtomicBool>,
}

impl Engine {
    /// Builds and starts the engine.
    ///
    /// Opens durable state through the keeper builder (coming up on persisted
    /// state), replays retained ledger entries to rebuild volatile state only when
    /// recovering from a rewound accountsdb, starts the live sequencer, and spawns
    /// the pacemaker using the builder's blockstore timing.
    pub async fn new(
        mut builder: KeeperBuilder,
        pacer: Option<ExternalPacer>,
        shutdown: &mut ShutdownManager,
    ) -> Result<Self> {
        let cache = Arc::new(ProgramCache::default());
        let cpus = (num_cpus::get().saturating_sub(2)).max(2);

        builder.builtins.insert(
            magic_root_interface::ID,
            (MagicRootEntrypoint::vm, MagicRootEntrypoint::codegen),
        );
        builder.builtins.insert(
            solana_system_program::id(),
            (
                SystemProgramEntrypoint::vm,
                SystemProgramEntrypoint::codegen,
            ),
        );

        for (&id, builtin) in &builder.builtins {
            let entry = ProgramCacheEntry::new_builtin(*builtin);
            cache.assign_program(id, entry.into());
        }
        let blockstore = builder.blockstore;
        let state = Arc::new(builder.build(shutdown).await?);
        Self::try_replay(&state, &cache, cpus).await?;
        let (service, sequencer) = Sequencer::new(cpus / 2, state.clone(), cache, shutdown, false)?;
        service.spawn()?;
        let terminating = Arc::new(AtomicBool::new(false));
        let engine = Self { state, sequencer, terminating };
        PaceMaker::spawn(engine.clone(), pacer, blockstore, shutdown)?;
        info!(authority = %engine.authority(), cpus, "engine started");
        Ok(engine)
    }

    /// Quiesces execution and closes durable state.
    ///
    /// `dump` serializes chain-mirrored state for an externally paced replica to
    /// restore on its next open. Internally paced leaders clear that state once
    /// during startup instead and only flush durable state here.
    pub async fn shutdown(&self, dump: bool) -> Result<()> {
        info!(dump, "shutting down the engine");
        self.terminating.store(true, Ordering::Release);
        let _guard = self.barrier().await?;
        if dump {
            self.accounts().dump(None).map_err(KeeperError::from)?;
        }
        self.sync(true).map_err(Into::into)
    }

    /// Returns an accessor for mutating the account at `pubkey`.
    pub fn account(&self, pubkey: Pubkey) -> AccountAccessor<'_> {
        AccountAccessor { engine: self, pubkey }
    }

    /// Returns an accessor for signing and submitting transactions.
    pub fn transaction<T>(&self, transaction: T) -> Result<TransactionAccessor<'_>>
    where
        T: IntoTransactionView,
    {
        let transaction = transaction.compose(self)?;
        Ok(TransactionAccessor { engine: self, transaction })
    }

    /// Drains in-flight execution and keeps the sequencer paused until the handle is dropped.
    pub async fn barrier(&self) -> Result<BarrierHandle> {
        let (controller, guard) = runtime::barrier();
        self.sequencer.send(SequencerMessage::Barrier(guard)).await?;
        controller.acknowledged.await?;
        Ok(controller.released)
    }

    /// Applies one retained ledger entry through the engine's ordered paths.
    ///
    /// Seal and reset entries quiesce execution before touching shared state;
    /// a reconstructed seal whose checksum differs returns
    /// [`ReplayError::StateMismatch`].
    pub async fn replay(&self, entry: OwnedBlockstoreEntry) -> Result<()> {
        match entry {
            OwnedBlockstoreEntry::Transaction(txn) => self.transaction(txn)?.schedule().await?,
            OwnedBlockstoreEntry::Block(block) => {
                self.sequencer.send(SequencerMessage::Block(block)).await?;
            }
            OwnedBlockstoreEntry::Superblock(expected) => {
                let _guard = self.barrier().await?;
                self.accounts().set_superblock(expected.id);
                self.sync(false)?;
                let observed = self.superblocks().sealed();
                if observed != expected {
                    error!(?observed, ?expected, "state mismatch; aborting replay");
                    Err(ReplayError::StateMismatch)?;
                }
            }
            OwnedBlockstoreEntry::Reset(slot) => {
                let _guard = self.barrier().await?;
                self.reset(slot)?;
            }
        };
        Ok(())
    }

    /// Rebuilds state through a temporary replay sequencer when accountsdb trails
    /// the retained ledger, then stops every temporary service before returning.
    async fn try_replay(state: &Arc<Keeper>, cache: &Arc<ProgramCache>, cpus: usize) -> Result<()> {
        let timer = Instant::now();
        let Some(mut replayer) = state.replay().await? else {
            return Ok(());
        };
        let mut shutdown = ShutdownManager::default();
        let mut sh = shutdown.handle(Service::LedgerReplayer);
        let (service, sequencer) =
            Sequencer::new(cpus, state.clone(), cache.clone(), &mut shutdown, true)?;
        service.spawn()?;
        let engine = Self {
            state: state.clone(),
            sequencer,
            terminating: Default::default(),
        };
        while let Some(entry) = replayer.rx.recv().await {
            engine.replay(entry).await?;
        }
        replayer
            .response
            .recv_timeout()
            .await
            .map_err(ReplayError::from)?
            .map_err(ReplayError::from)?;

        drop(engine.barrier().await?);
        engine.sync(false)?;

        let slot = state.blocks().latest().slot;
        info!(slot, duration = ?timer.elapsed(), "ledger replay complete");
        sh.terminate(ShutdownReason::Signalled);
        shutdown.terminate().await;
        Ok(())
    }
}
