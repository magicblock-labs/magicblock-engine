#![doc = include_str!("../README.md")]

use std::{
    fs::{self, File},
    path::PathBuf,
    sync::Arc,
    thread,
};

use agave_feature_set::FeatureSet;
use solana_account::AccountBuilder;
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use accountsdb::{AccountsDB, SnapshotError};
use ledger::{
    LedgerHandle, Superblock,
    request::{ReadRequest, ReplayHandle, ReplayParams, RequestPayload},
    schema::Event,
};
use nucleus::{
    Slot,
    config::Authority,
    ledger::{ACCOUNTSDB_SNAPSHOT_FILE, SuperblockSeal},
};
use solana_sysvar::rent::Rent;

use crate::{
    accessor::{AccountsAccessor, BlocksAccessor, SuperblockAccessor, TransactionsAccessor},
    builder::SPONSOR_INIT_BALANCE,
    cache::Caches,
    error::Result,
    metrics::Operation,
    subscriptions::Subscriptions,
};

pub use cache::{AccountLoad, AccountWait, MissingAccount};
/// Re-exported so callers can name what `Keeper::transactions().status()` returns.
pub use ledger::request::TransactionStatus;
pub use nucleus::runtime::{
    ExecutionRecord, FullTransaction, ResolvedTransaction, TransactionView,
};

mod accessor;
pub mod builder;
mod cache;
pub mod error;
mod metrics;
mod subscriptions;
mod util;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

#[cfg(test)]
mod tests;

/// Owns the durable state and live access helpers for the execution engine.
pub struct Keeper {
    /// Local signer and optional remote authority represented by this engine.
    authority: Authority,
    /// Active feature set governing runtime behavior.
    featureset: FeatureSet,
    /// Rent parameters applied during execution.
    rent: Rent,
    /// Account state store.
    accountsdb: AccountsDB,
    /// Ledger worker handles and append path.
    ledger: LedgerHandle,
    /// Read-side caches shared by accessors.
    caches: Caches,
    /// Subscription fanout maps for live updates.
    subscriptions: Arc<Subscriptions>,
}

impl Keeper {
    /// Returns the account operations namespace.
    pub fn accounts(&self) -> AccountsAccessor<'_> {
        AccountsAccessor { keeper: self }
    }

    /// Returns the transaction operations namespace.
    pub fn transactions(&self) -> TransactionsAccessor<'_> {
        TransactionsAccessor { keeper: self }
    }

    /// Returns the block operations namespace.
    pub fn blocks(&self) -> BlocksAccessor<'_> {
        BlocksAccessor { keeper: self }
    }

    /// Returns the superblock operations namespace.
    pub fn superblocks(&self) -> SuperblockAccessor<'_> {
        SuperblockAccessor { keeper: self }
    }

    /// Returns the configured remote authority, or the local identity when unset.
    pub fn authority(&self) -> Pubkey {
        self.authority.pubkey()
    }

    /// Returns the local signer, which may differ from [`Self::authority`].
    pub fn signer(&self) -> &Keypair {
        &self.authority.local
    }

    /// Returns the latest block hash
    pub fn blockhash(&self) -> Hash {
        self.blocks().latest().hash
    }

    /// Borrows the handle used for direct ledger reads, appends, and
    /// durable-position subscriptions.
    pub fn ledger(&self) -> &LedgerHandle {
        &self.ledger
    }

    /// Streams retained ledger entries after accountsdb's sealed superblock up to
    /// the ledger tip, used to rebuild state after snapshot restoration. Returns
    /// `None` when accountsdb is already current by slot.
    pub async fn replay(&self) -> Result<Option<ReplayHandle>> {
        let ledger = self.ledger.tip().unwrap_or_default();
        let accountsdb = self.accountsdb.slot();
        if accountsdb >= ledger {
            return Ok(None);
        };
        let (tx, rx) = mpsc::channel(16);
        let params = ReplayParams {
            tx,
            superblock: self.accountsdb.superblock(),
        };
        let (payload, response) = RequestPayload::new(params);
        let handle = ReplayHandle { rx, response };
        self.ledger.reader.send_async(ReadRequest::Replay(payload)).await?;
        warn!(accountsdb, ledger, "starting ledger replay (slot lag)");
        Ok(Some(handle))
    }

    /// Seal the current superblock and archive the matching accounts snapshot.
    ///
    /// Must run only when no account store can race the snapshot export; the
    /// in-body `SAFETY` note relies on this exclusivity.
    pub fn finalize_superblock(&self) -> Result<()> {
        let _timer = metrics::time(Operation::FinalizeSuperblock);
        let head = self.ledger.head();
        let next = head + 1;
        // SAFETY: `snapshot` requires exclusive write access to accountsdb,
        // i.e. no store operation may race the export. `finalize_superblock`
        // is only run when there're no concurrent mutations taking place
        let snapshot = unsafe { self.accountsdb.snapshot(head) }?;
        let checksum = self.accountsdb.checksum();
        let seal = SuperblockSeal { id: head, checksum };
        self.superblocks().append(seal)?;
        let dir = Superblock::init_dir(&self.ledger.directory, next)?;
        self.archive(snapshot, dir)?;
        info!(head, "finalized superblock");
        Ok(())
    }

    /// Resolved once from the seeded feature accounts at startup and fixed for
    /// the engine's lifetime — features never activate mid-run.
    pub fn features(&self) -> &FeatureSet {
        &self.featureset
    }

    /// Supplied by the builder at startup and fixed for the engine's lifetime;
    /// the same parameters that sized the seeded accounts.
    pub fn rent(&self) -> &Rent {
        &self.rent
    }

    /// Waits for queued ledger work to become durable, then synchronously
    /// flushes persisted account storage. Volatile accounts are not serialized.
    ///
    /// A final sync closes every ledger reader and the appender. It is
    /// irreversible and must only be used during coordinated shutdown.
    pub fn sync(&self, is_final: bool) -> Result<()> {
        if is_final {
            for _ in 0..self.ledger.reader.receiver_count() {
                self.ledger.reader.send(ReadRequest::Shutdown)?;
            }
        }
        self.superblocks().sync(is_final)?;
        self.accountsdb.flush(true).map_err(Into::into)
    }

    /// Appends a reset marker before discarding chain-synchronized volatile
    /// accounts and restoring the authority sponsor's initial balance.
    ///
    /// Internal system accounts and persisted engine-authoritative state remain
    /// available.
    pub fn reset(&self, slot: Slot) -> Result<()> {
        self.ledger.appender.send(Event::Reset(slot))?;
        self.accountsdb.reset();
        if let Some(authority) = self.accounts().get(&self.authority())? {
            let acc = AccountBuilder::from(authority).lamports(SPONSOR_INIT_BALANCE);
            self.accounts().store(&[(self.authority(), acc.build())])?;
        }
        info!(slot, "reset volatile state");
        Ok(())
    }

    /// Spawns a background thread that tars and zstd-compresses the accountsdb
    /// snapshot at `snapshot` into `target`, removing the snapshot afterward.
    fn archive(
        &self,
        snapshot: PathBuf,
        target: PathBuf,
    ) -> std::result::Result<(), SnapshotError> {
        let path = target.join(ACCOUNTSDB_SNAPSHOT_FILE);
        let tmp = target.join(format!("{ACCOUNTSDB_SNAPSHOT_FILE}.tmp"));
        let dst = File::options().write(true).create(true).truncate(true).open(&tmp)?;
        let snapshots = self.subscriptions.snapshots.clone();
        thread::Builder::new().name("snapshot-archiver".into()).spawn(move || {
            {
                let _timer = metrics::time(Operation::ArchiveSnapshot);
                let mut tar = tar::Builder::new(zstd::Encoder::new(dst, 0)?);
                tar.append_dir_all(".", &snapshot)?;
                {
                    let archive = tar.into_inner()?.finish()?;
                    metrics::snapshot_size(archive.metadata()?.len());
                    archive.sync_data()?;
                }
                // Rename only after sync so replication cannot serve a partial archive.
                fs::rename(tmp, &path)?;
                fs::remove_dir_all(snapshot)?;
                if snapshots.receiver_count() != 0 {
                    let _ = snapshots.send(path);
                }
                Ok::<(), SnapshotError>(())
            }
            .inspect_err(|error| error!(?error, "snapshot archival failed"))
        })?;
        Ok(())
    }
}
