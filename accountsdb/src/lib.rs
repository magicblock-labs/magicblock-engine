#![doc = include_str!("../README.md")]

use std::{
    cell::RefCell,
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::atomic::Ordering::*,
};

use derive_more::From;
use nucleus::Slot;
use nucleus::heed::RoTxnTls;
use solana_account::{AccountSeqLock, AccountSharedData, CoWAccount};
use solana_pubkey::Pubkey;
use tracing::{info, warn};

use crate::{
    store::{DatabaseVersion, PersistedProgramIter, PersistedStore},
    volatile::VolatileStore,
};

pub use snapshot::{BackupOp, SnapshotError, SnapshotResult};
pub use store::mmap::STORAGE_FILE;

mod metrics;
mod snapshot;
mod store;
mod volatile;

#[cfg(test)]
mod tests;

/// Active database subdirectory.
const ACTIVE_DIR: &str = "CURRENT";

/// Top-level account store backed by persisted and volatile backends.
pub struct AccountsDB {
    /// On-disk store for engine-authoritative account modes.
    persisted: PersistedStore,
    /// Rebuildable in-memory store for non-authoritative account modes.
    volatile: VolatileStore,
    /// Database root directory.
    root: PathBuf,
}

impl AccountsDB {
    /// Opens or creates the database at `root`.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_owned();
        let path = Self::directory(&root);
        let persisted = PersistedStore::new(&path)?;
        let volatile = VolatileStore::new(&path)?;
        info!(?path, "opened accountsdb");
        let db = Self { persisted, volatile, root };
        metrics::init(&db);
        Ok(db)
    }

    /// Returns the active database directory under `root`.
    pub fn directory(root: &Path) -> PathBuf {
        root.join(ACTIVE_DIR)
    }

    /// Stores accounts in the backend that matches their current form.
    ///
    /// Persistent modes are kept in persisted storage. Other modes are kept in
    /// volatile storage. Each batch also touches the opposite backend so stale
    /// copies are removed after mode changes. Persisted failures roll back
    /// borrowed images before the caller sees the error.
    pub fn store<'a, AC>(&self, accounts: AC) -> Result<()>
    where
        AC: IntoIterator<Item = &'a AccountEntry> + Clone,
        <AC as IntoIterator>::IntoIter: Clone,
    {
        let iter = accounts.clone().into_iter().filter(persisted);
        self.persisted.upsert(iter)?;

        let iter = accounts.into_iter().filter(volatile);
        self.volatile.upsert(iter);

        Ok(())
    }

    /// Commits one ledger transaction's account transitions.
    ///
    /// The transaction count advances only after every supplied transition is
    /// stored successfully. Empty transitions count, including failed SVM
    /// executions that reached the commit path without account writes.
    pub fn commit<'a, AC>(&self, accounts: AC) -> Result<()>
    where
        AC: IntoIterator<Item = &'a AccountEntry> + Clone,
        <AC as IntoIterator>::IntoIter: Clone,
    {
        self.store(accounts)?;
        self.persisted.meta().transactions.fetch_add(1, Release);
        Ok(())
    }

    /// Creates a loader that reuses a read transaction for persisted lookups.
    pub fn loader(&self) -> AccountLoader<'_> {
        AccountLoader::new(self)
    }

    /// Iterates program-owned accounts across both backends.
    pub fn program(&self, owner: &Pubkey) -> Result<ProgramIter<'_>> {
        let persisted = self.persisted.program(*owner)?;
        let volatile = self.volatile.program(owner);
        Ok(ProgramIter { persisted, volatile, db: self })
    }

    /// Returns the latest slot persisted in the database metadata.
    pub fn slot(&self) -> Slot {
        self.persisted.meta().slot.load(Acquire)
    }

    /// Sets the database slot and flushes dirty pages asynchronously.
    pub fn set_slot(&self, slot: Slot) -> Result<()> {
        self.persisted.meta().slot.store(slot, Release);
        self.flush(false)
    }

    /// Returns the id of the last sealed superblock recorded in the database metadata.
    pub fn superblock(&self) -> Slot {
        self.persisted.meta().superblock.load(Acquire)
    }

    /// Returns the number of successfully committed ledger transactions.
    pub fn transactions(&self) -> u64 {
        self.persisted.meta().transactions.load(Acquire)
    }

    /// Records the last sealed superblock id. Set on snapshot, and on replay
    /// before recomputing the checksum to compare against a seal.
    pub fn set_superblock(&self, superblock: u64) {
        self.persisted.meta().superblock.store(superblock, Release);
    }

    /// Flushes persisted account storage, forcing synchronous durability when requested.
    pub fn flush(&self, force: bool) -> Result<()> {
        self.persisted.flush(force).map_err(Into::into)
    }

    /// Validates the persisted store checksum and on-disk format version.
    pub fn validate(&self) -> Result<()> {
        self.persisted.validate()
    }

    /// Returns the last checksum published on superblock boundary.
    pub fn checksum(&self) -> u64 {
        self.persisted.meta().checksum.load(Acquire)
    }

    /// Drops chain-mirrored volatile state while retaining system accounts;
    /// persisted state is left untouched.
    ///
    /// Chain-owned accounts can be fetched again when synchronization resumes.
    /// System accounts hold internal runtime state and survive the reset; their
    /// volatile owner indexes are rebuilt. Persisted, engine-authoritative state
    /// is never reset.
    pub fn reset(&self) {
        self.volatile.reset();
    }
}

/// Loader that caches a read transaction for persisted account lookups.
pub struct AccountLoader<'a> {
    /// Cached read transaction for the persisted index.
    txn: RefCell<Option<RoTxnTls<'a>>>,
    /// Database handle used for volatile and persisted lookups.
    db: &'a AccountsDB,
}

impl<'a> AccountLoader<'a> {
    /// Creates a new loader bound to `db`.
    pub fn new(db: &'a AccountsDB) -> Self {
        Self { txn: Default::default(), db }
    }

    /// Loads one account, reusing the persisted read transaction across calls.
    ///
    /// Reuse the loader for batch lookups to keep them on the same persisted
    /// index snapshot. Persisted accounts take precedence over volatile ones.
    pub fn load(&self, pubkey: &Pubkey) -> Result<Option<AccountSharedData>> {
        let txn = &mut self.txn.borrow_mut();
        if let Some(acc) = self.db.persisted.load(txn, pubkey)? {
            metrics::load(StoreKind::Persisted);
            return Ok(Some(acc.into()));
        }
        let account = self.db.volatile.load(pubkey).map(Into::into);
        if account.is_some() {
            metrics::load(StoreKind::Volatile);
        } else {
            metrics::load(StoreKind::Absent);
        }
        Ok(account)
    }

    /// Applies `reader` to an account image stable across a concurrent publish.
    ///
    /// Prefer this over [`Self::load`] when reading fields from persisted
    /// accounts that may be updated concurrently. The reader may be called more
    /// than once when the borrowed image changes, so it should have no side
    /// effects.
    pub fn read<F, R>(&self, pubkey: &Pubkey, reader: F) -> Result<Option<R>>
    where
        F: Fn(&AccountSharedData) -> R,
    {
        let Some(account) = self.load(pubkey)? else {
            return Ok(None);
        };
        Ok(Some(AccountSeqLock::new(account).read(reader)))
    }

    /// Returns whether an account exists in either backend.
    pub fn contains(&self, pubkey: &Pubkey) -> Result<bool> {
        let txn = &mut self.txn.borrow_mut();
        if self.db.persisted.contains(txn, pubkey)? {
            return Ok(true);
        }
        let contains = self.db.volatile.contains(pubkey);
        Ok(contains)
    }
}

/// Iterates program-owned accounts across both backends.
pub struct ProgramIter<'a> {
    /// Persisted program accounts.
    persisted: Option<PersistedProgramIter<'a>>,
    /// Volatile program pubkeys.
    volatile: BTreeSet<Pubkey>,
    /// Database handle used to resolve volatile accounts.
    db: &'a AccountsDB,
}

impl<'a> Iterator for ProgramIter<'a> {
    type Item = AccountEntry;
    /// Yields authoritative accounts first, then volatile ones.
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(persisted) = &mut self.persisted {
            // Yield authoritative entries first.
            if let Some(item) = persisted.next() {
                return Some(item);
            }
        }
        // Release the persisted read txn before draining volatile entries.
        let _ = self.persisted.take();
        // Then drain the in-memory set of non-authoritative accounts.
        while let Some(pubkey) = self.volatile.pop_first() {
            if let Some(account) = self.db.volatile.load(&pubkey) {
                return Some((pubkey, account.into()));
            }
            warn!(%pubkey, "volatile program set references a missing account; skipping");
        }
        None
    }
}

/// Errors returned by accountsdb.
#[derive(Debug, thiserror::Error, From)]
pub enum AccountsDBError {
    /// LMDB key-value codec error.
    #[error("LMDB key/value codec error: {0}")]
    Codec(#[source] heed::BoxedError),
    /// Filesystem error.
    #[error("filesystem I/O error: {0}")]
    IO(#[source] std::io::Error),
    /// LMDB index access error.
    #[error("LMDB index error: {0}")]
    Index(#[source] heed::Error),
    /// Storage allocation would exceed the maximum mapped size.
    #[error("mapped storage exceeded the 32 GiB limit")]
    Allocation,
    /// Opened database version is not supported by current implementation.
    #[error("unsupported database version: {0:?}")]
    UnsupportedVersion(DatabaseVersion),
    /// Database was corrupted during the shutdown/crash.
    #[error("database integrity check failed")]
    Corruption,
    /// Volatile snapshot serialization error.
    #[error("volatile snapshot serialization error: {0}")]
    Serde(#[source] Box<bincode::ErrorKind>),
}

/// Result type used by the accountsdb crate.
type Result<T> = std::result::Result<T, AccountsDBError>;
/// Account key plus shared account payload.
pub type AccountEntry = (Pubkey, AccountSharedData);

/// Classification used by accountsdb metrics.
#[derive(Clone, Copy)]
pub(crate) enum StoreKind {
    /// Mmap-backed persisted storage.
    Persisted,
    /// In-memory volatile storage.
    Volatile,
    /// Account was absent from both storage backends.
    Absent,
}

impl StoreKind {
    /// Returns the Prometheus label value for this classification.
    pub(crate) fn label(self) -> &'static str {
        match self {
            StoreKind::Persisted => "persisted",
            StoreKind::Volatile => "volatile",
            StoreKind::Absent => "absent",
        }
    }
}

/// Returns `true` for entries that must touch persisted storage.
fn persisted(entry: &&AccountEntry) -> bool {
    match entry.1.cow() {
        CoWAccount::Borrowed(_) => true,
        CoWAccount::Owned(_) => entry.1.mode().authoritative(),
    }
}

/// Returns `true` for entries that must touch volatile storage.
fn volatile(entry: &&AccountEntry) -> bool {
    match entry.1.cow() {
        CoWAccount::Borrowed(_) => !entry.1.mode().authoritative(),
        CoWAccount::Owned(_) => true,
    }
}
