//! Shared black-box harness for engine-backed integration suites.
//!
//! Builds a real [`Engine`] over [`keeper::testkit`] directories with internal or
//! externally controlled pacing. Compiled only under the `testkit` feature.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{path::PathBuf, sync::Arc, time::Duration};

use derive_more::Deref;
use keeper::{
    ExecutionRecord,
    builder::KeeperBuilder,
    testkit::{Dirs, SUPERBLOCK, block, keeper_builder, seal_and_archive_with},
};
use nucleus::{Slot, config::Authority, ledger::BlockstorePosition, shutdown::ShutdownManager};
use solana_account::AccountSharedData;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_transaction::TransactionResult;
use tokio::{sync::mpsc, time};

use crate::{Engine, IntoTransactionView, pacemaker::ExternalBlock};

const TIMEOUT: Duration = Duration::from_secs(4);

/// Block pacing for a [`TestEngine`].
pub enum Pacing {
    /// The test supplies blocks through [`TestEngine::pacer`].
    External,
    /// The engine runs its own pacemaker.
    Internal,
}

/// A running engine plus its deterministic pacing and lifecycle handles.
#[derive(Deref)]
pub struct TestEngine {
    #[deref]
    engine: Engine,
    shutdown: ShutdownManager,
    authority: Authority,
    dirs: Dirs,
    pacer: Option<mpsc::Sender<ExternalBlock>>,
    slot: Slot,
}

impl TestEngine {
    /// Starts the standard test engine on fresh directories.
    pub async fn new() -> Self {
        Self::with(Dirs::default(), Arc::new(Keypair::new())).await
    }

    /// Starts the standard test engine over `dirs` with `authority`.
    pub async fn with(dirs: Dirs, authority: impl Into<Authority>) -> Self {
        Self::try_with(dirs, authority).await.unwrap()
    }

    /// Fallible [`Self::with`], used when startup failure is the assertion.
    pub async fn try_with(dirs: Dirs, authority: impl Into<Authority>) -> crate::Result<Self> {
        let mut builder = keeper_builder(&dirs);
        builder.authority = authority.into();
        Self::try_from_builder(dirs, builder, Pacing::External).await
    }

    /// Starts an engine from a caller-configured keeper builder.
    ///
    /// `dirs` must own the directories referenced by `builder` and outlive the
    /// resulting engine.
    pub async fn from_builder(dirs: Dirs, builder: KeeperBuilder, pacing: Pacing) -> Self {
        Self::try_from_builder(dirs, builder, pacing).await.unwrap()
    }

    /// Fallible [`Self::from_builder`].
    pub async fn try_from_builder(
        dirs: Dirs,
        builder: KeeperBuilder,
        pacing: Pacing,
    ) -> crate::Result<Self> {
        let authority = builder.authority.clone();
        let (pacer, rx) = match pacing {
            Pacing::External => {
                let (tx, rx) = mpsc::channel(64);
                (Some(tx), Some(rx))
            }
            Pacing::Internal => (None, None),
        };
        let mut shutdown = ShutdownManager::default();
        let engine = Engine::new(builder, rx, &mut shutdown).await?;
        let slot = engine.blocks().current_slot();
        Ok(Self {
            engine,
            shutdown,
            authority,
            dirs,
            pacer,
            slot,
        })
    }

    /// Cloneable external pacemaker sender for services under test.
    ///
    /// # Panics
    ///
    /// Panics if the engine is internally paced.
    pub fn pacer(&self) -> mpsc::Sender<ExternalBlock> {
        self.pacer.clone().expect("engine is externally paced")
    }

    /// Mutable lifecycle manager used to register or await test services.
    pub fn shutdown(&mut self) -> &mut ShutdownManager {
        &mut self.shutdown
    }

    /// Drains engine work, flushes queued ledger appends, and returns the durable cursor.
    pub async fn sync(&self) -> BlockstorePosition {
        drop(self.barrier().await.unwrap());
        self.superblocks().sync(false).unwrap();
        self.superblocks().position()
    }

    /// Full committed account, or `None` when absent/closed.
    pub fn get_account(&self, key: Pubkey) -> Option<AccountSharedData> {
        self.engine.accounts().loader().load(&key).unwrap()
    }

    /// Executes instructions and returns the committed transaction result.
    pub async fn execute(&self, txn: impl IntoTransactionView) -> TransactionResult<()> {
        self.transaction(txn).unwrap().execute().await.unwrap()
    }

    /// Simulates instructions without committing them.
    pub async fn simulate(
        &self,
        txn: impl IntoTransactionView,
    ) -> TransactionResult<ExecutionRecord> {
        self.transaction(txn).unwrap().simulate().await.unwrap()
    }

    /// Schedules instructions without awaiting commit.
    pub async fn schedule(&self, txn: impl IntoTransactionView) {
        self.transaction(txn).unwrap().schedule().await.unwrap();
    }

    /// Advances `n` block boundaries.
    ///
    /// # Panics
    ///
    /// Panics if the engine is internally paced.
    pub async fn advance(&mut self, n: u64) {
        for _ in 0..n {
            let (block, submitted) = ExternalBlock::new(block(self.slot));
            self.pacer().send(block).await.unwrap();
            time::timeout(TIMEOUT, submitted)
                .await
                .expect("pacemaker accepts the block in time")
                .expect("pacemaker reports block submission");
            self.slot += 1;
        }
    }

    /// Seals the next superblock and waits for its archive and durable rotation.
    pub async fn seal_and_archive(&mut self) -> PathBuf {
        let boundary = self.slot.next_multiple_of(SUPERBLOCK.into());
        let engine = self.engine.clone();
        seal_and_archive_with(&engine, || async {
            while self.slot <= boundary {
                self.advance(1).await;
            }
        })
        .await
    }

    /// Stops every service and returns the directories and authority for reopen.
    pub async fn close(self) -> (Dirs, Authority) {
        let Self {
            mut shutdown, dirs, authority, ..
        } = self;
        shutdown.terminate().await;
        (dirs, authority)
    }
}
