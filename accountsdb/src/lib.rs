#![doc = include_str!("../README.md")]

use std::{collections::BTreeSet, path::PathBuf, sync::atomic::Ordering};

use solana_account::{AccountSharedData, CoWAccount};
use solana_pubkey::Pubkey;

use crate::{
    snapshot::SnapshotError,
    store::{DatabaseVersion, PersistedProgramIter, PersistedStore, index::RoTxnTls},
    volatile::VolatileStore,
};

mod snapshot;
mod store;
mod volatile;

/// Active database subdirectory.
const ACTIVE_DIR: &str = "CURRENT";

/// Top-level account store backed by persisted and volatile backends.
pub struct AccountsDB {
    /// Authoritative on-disk store for mutable accounts.
    persisted: PersistedStore,
    /// Authoritative in-memory store for immutable accounts.
    volatile: VolatileStore,
    /// Database directory where database files are stored
    root: PathBuf,
}

impl AccountsDB {
    /// Opens or creates the database at `root`.
    pub fn new(root: PathBuf) -> Result<Self> {
        let path = root.join(ACTIVE_DIR);
        let persisted = PersistedStore::new(&path)?;
        let volatile = VolatileStore::new(&path)?;
        Ok(Self { persisted, volatile, root })
    }

    /// Returns `true` for entries that belong in persisted storage.
    fn is_persisted(entry: &&AccountEntry) -> bool {
        match entry.1.cow() {
            CoWAccount::Borrowed(_) => true,
            CoWAccount::Owned(_) => entry.1.mutable(),
        }
    }

    /// Returns `true` for entries that belong in volatile storage.
    fn is_volatile(entry: &&AccountEntry) -> bool {
        match entry.1.cow() {
            CoWAccount::Borrowed(_) => !entry.1.mutable(),
            CoWAccount::Owned(_) => true,
        }
    }

    /// Stores accounts in the backend that matches their current form.
    ///
    /// Mutable accounts are kept in persisted storage. Immutable accounts are
    /// kept in volatile storage. Each batch also touches the opposite backend
    /// so stale copies are removed after mode changes. Persisted failures roll
    /// back borrowed images before the caller sees the error.
    pub fn store<'a, AI>(&self, accounts: AI) -> Result<()>
    where
        AI: IntoIterator<Item = &'a AccountEntry> + Clone,
        <AI as IntoIterator>::IntoIter: Clone,
    {
        let iter = accounts.clone().into_iter().filter(Self::is_persisted);
        self.persisted.upsert(iter)?;

        let iter = accounts.into_iter().filter(Self::is_volatile);
        self.volatile.upsert(iter);

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

    /// Sets the database slot and flushes dirty pages asynchronously.
    pub fn set_slot(&self, slot: u64) -> Result<()> {
        self.persisted.storage.meta().slot.store(slot, Ordering::Release);
        self.persisted.flush(false).map_err(Into::into)
    }
}

/// Loader that caches a read transaction for persisted account lookups.
pub struct AccountLoader<'a> {
    /// Cached read transaction for the persisted index.
    txn: Option<RoTxnTls<'a>>,
    /// Database handle used for volatile and persisted lookups.
    db: &'a AccountsDB,
}

impl<'a> AccountLoader<'a> {
    /// Creates a new loader bound to `db`.
    pub fn new(db: &'a AccountsDB) -> Self {
        Self { txn: None, db }
    }

    /// Loads one account, preferring volatile storage.
    pub fn load(&mut self, pubkey: &Pubkey) -> Result<Option<AccountSharedData>> {
        if let Some(acc) = self.db.volatile.load(pubkey) {
            return Ok(Some(acc.into()));
        }
        let acc = self.db.persisted.load(&mut self.txn, pubkey)?;
        Ok(acc.map(Into::into))
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
    /// Yields persisted mutable accounts first, then volatile immutable ones.
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(persisted) = &mut self.persisted {
            // Yield persisted entries first; mutable accounts live there.
            if let Some(item) = persisted.next() {
                return Some(item);
            }
        }
        // Release the persisted read txn before draining volatile entries.
        let _ = self.persisted.take();
        // Then drain the in-memory set of immutable accounts.
        while let Some(pubkey) = self.volatile.pop_first() {
            if let Some(account) = self.db.volatile.load(&pubkey) {
                return Some((pubkey, account.into()));
            }
        }
        None
    }
}

/// Errors returned by accountsdb.
#[derive(Debug, thiserror::Error)]
pub enum AccountsDBError {
    /// LMDB key-value codec error.
    #[error("LMDB key/value codec error: {0}")]
    Codec(#[from] heed::BoxedError),
    /// Filesystem error.
    #[error("filesystem I/O error: {0}")]
    IO(#[from] std::io::Error),
    /// LMDB index access error.
    #[error("LMDB index error: {0}")]
    Index(#[from] heed::Error),
    /// Storage allocation would exceed the maximum mapped size.
    #[error("mapped storage exceeded the 32 GiB limit")]
    Allocation,
    /// Opened database version is not supported by current implementation.
    #[error("unsupported database version: {0:?}")]
    UnsupportedVersion(DatabaseVersion),
    /// Database was corrupted during the shutdown/crash.
    #[error("database integrity check failed")]
    Corruption,
    /// Snapshot import/export failed.
    #[error("snapshot export failed")]
    Snapshot(#[from] SnapshotError),
}

/// Result type used by the accountsdb crate.
type Result<T> = std::result::Result<T, AccountsDBError>;
/// Account key plus shared account payload.
type AccountEntry = (Pubkey, AccountSharedData);
