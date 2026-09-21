#![doc = include_str!("../README.md")]

use std::{
    cell::RefCell,
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::atomic::Ordering::*,
};

use derive_more::From;
use nucleus::Slot;
use solana_account::{AccountMode, AccountSeqLock, AccountSharedData, CoWAccount};
use solana_pubkey::Pubkey;
use tracing::{info, warn};

use crate::{
    readers::{ReadGuard, Readers},
    store::{DatabaseVersion, PersistedProgramIter, PersistedStore, index::RoTxnTls},
    volatile::VolatileStore,
};

pub use snapshot::{BackupOp, SnapshotError, SnapshotResult};
pub use store::mmap::STORAGE_FILE;

mod metrics;
mod readers;
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
    /// Reader admission while the persisted layout is being relocated.
    readers: Readers,
}

impl AccountsDB {
    /// Opens or creates the database at `root`.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let readers = Readers::new()?;
        let root = root.as_ref().to_owned();
        let path = Self::directory(&root);
        let persisted = PersistedStore::new(&path)?;
        let volatile = VolatileStore::new(&path)?;
        info!(?path, "opened accountsdb");
        let db = Self {
            persisted,
            volatile,
            root,
            readers,
        };
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

    /// Reads each program-owned account without letting borrowed images escape.
    ///
    /// The iterator retains reader admission and the persisted index snapshot.
    /// `reader` may run again after a concurrent image publish and must have no
    /// side effects. Only its result escapes; copying account data is optional.
    pub fn program<'a, F, R>(
        &'a self,
        owner: &Pubkey,
        reader: F,
    ) -> Result<impl Iterator<Item = (Pubkey, R)> + 'a>
    where
        F: Fn(&Pubkey, &AccountSharedData) -> R + 'a,
        R: 'a,
    {
        let guard = self.readers.enter();
        let iter = ProgramIter {
            persisted: self.persisted.program(*owner)?,
            volatile: self.volatile.program(owner),
            db: self,
            _reader: guard,
        };
        Ok(iter.map(move |(pubkey, account)| {
            let result = AccountSeqLock::new(account).read(|account| reader(&pubkey, account));
            (pubkey, result)
        }))
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
        // A synchronous flush walks indexed account images for the checksum.
        let _reader = force.then(|| self.readers.enter());
        self.persisted.flush(force).map_err(Into::into)
    }

    /// Validates the persisted store checksum and on-disk format version.
    pub fn validate(&self) -> Result<()> {
        let _reader = self.readers.enter();
        self.persisted.validate()
    }

    /// Compacts persisted storage to a non-overlapping packing fixed point.
    ///
    /// This must run only after validation and before loaders or iterators are
    /// created. Vacated sources become eligible on the following pass, and all
    /// successful passes are flushed synchronously before returning.
    /// Returns the total number of reclaimed 8-byte storage units.
    pub fn compact(&mut self) -> Result<u64> {
        let mut reclaimed = 0;
        let mut changed = false;
        loop {
            // SAFETY: `&mut self` excludes readers and writers through this handle;
            // the store owns its LMDB environment and mapped storage.
            let pass = unsafe { self.persisted.defragment() }?;
            reclaimed += pass.reclaimed;
            changed |= pass.changed();
            if !pass.changed() {
                break;
            }
        }
        if changed {
            self.flush(true)?;
        }
        Ok(reclaimed)
    }

    /// Returns the last checksum published on superblock boundary.
    pub fn checksum(&self) -> u64 {
        self.persisted.meta().checksum.load(Acquire)
    }

    /// Computes the current persisted-state checksum without flushing storage or
    /// updating the cached checksum. Volatile accounts are not included.
    ///
    /// # Safety
    /// The caller must exclude account writes and metadata changes for the scan.
    /// The reader scope excludes relocation while indexed images are borrowed.
    pub unsafe fn compute_checksum(&self) -> Result<u64> {
        let _reader = self.readers.enter();
        self.persisted.checksum().map_err(Into::into)
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

/// Synchronous reader scope caching a persisted index transaction.
///
/// Holding a guarded loader delays compaction. Finish the batch and drop it before
/// awaiting unrelated work. [`Self::read`] keeps account access within the scope;
/// raw execution views must obey [`Self::load`]'s safety contract.
/// Execution with its own compaction barrier can use [`Self::unguarded`].
pub struct AccountLoader<'a> {
    /// Cached read transaction for the persisted index.
    txn: RefCell<Option<RoTxnTls<'a>>>,
    /// Database handle used for volatile and persisted lookups.
    db: &'a AccountsDB,
    /// Present for ordinary readers, absent when the caller excludes compaction.
    /// Drops after the cached transaction so old offsets cannot escape admission.
    _reader: Option<ReadGuard<'a>>,
}

impl<'a> AccountLoader<'a> {
    /// Creates a new loader bound to `db`.
    #[inline]
    pub fn new(db: &'a AccountsDB) -> Self {
        Self {
            txn: RefCell::new(None),
            db,
            _reader: Some(db.readers.enter()),
        }
    }

    /// Creates a loader without reader-admission bookkeeping.
    ///
    /// Intended for execution already covered by the sequencer's barrier. It
    /// uses the same account lookup and cached index transaction as [`Self::new`].
    ///
    /// # Safety
    /// The caller must independently exclude compaction for this loader's
    /// entire lifetime, including its cached index transaction, and until all
    /// returned borrowed accounts have had their final access.
    #[inline]
    pub unsafe fn unguarded(db: &'a AccountsDB) -> Self {
        Self {
            txn: RefCell::new(None),
            db,
            _reader: None,
        }
    }

    /// Loads a zero-copy account view for externally synchronized execution.
    ///
    /// The SVM retains these views through transaction commit. Ordinary readers
    /// must use [`Self::read`] instead. Both paths reuse the same cached index
    /// transaction and give volatile accounts precedence over persisted ones.
    ///
    /// # Safety
    /// The database must outlive borrowed results. Retain a guarded loader until
    /// their last access, or independently exclude relocation for their use.
    /// Concurrent image updates require `AccountSeqLock`; account deletion and
    /// storage reuse must also be excluded while borrowed results are used.
    /// Mutating borrowed results additionally requires exclusive account access.
    pub unsafe fn load(&self, pubkey: &Pubkey) -> Result<Option<AccountSharedData>> {
        if let Some(account) = self.db.volatile.load(pubkey) {
            return Ok(Some(account.into()));
        }
        self.persisted(pubkey)
    }

    /// Resolves a persisted view using this scope's cached index transaction.
    /// Borrowed results must remain within the same boundary as `load` results.
    fn persisted(&self, pubkey: &Pubkey) -> Result<Option<AccountSharedData>> {
        let txn = &mut self.txn.borrow_mut();
        let account = self.db.persisted.load(txn, pubkey)?;
        Ok(account.map(Into::into))
    }

    /// Applies `reader` to an account image stable across a concurrent publish.
    ///
    /// The reader may be called more than once when the borrowed image changes,
    /// so it should have no side effects. Only its result escapes the scope;
    /// clone the account inside the callback when an owned snapshot is needed.
    pub fn read<F, R>(&self, pubkey: &Pubkey, reader: F) -> Result<Option<R>>
    where
        F: Fn(&AccountSharedData) -> R,
    {
        // SAFETY: the callback and sequence checks finish within this loader's
        // admission scope, or the caller's unguarded-construction contract.
        let account = unsafe { self.load(pubkey) }?;
        Ok(account.map(|account| AccountSeqLock::new(account).read(reader)))
    }

    /// Reads only the mode, without cloning volatile data or copying persisted data.
    /// Uses the same backend precedence and cached index snapshot as [`Self::read`].
    /// Persisted mode reads retry if a concurrent publish changes the image.
    pub fn mode(&self, pubkey: &Pubkey) -> Result<Option<AccountMode>> {
        if let Some(mode) = self.db.volatile.mode(pubkey) {
            return Ok(Some(mode));
        }
        Ok(self
            .persisted(pubkey)?
            .map(|account| AccountSeqLock::new(account).read(AccountSharedData::mode)))
    }

    /// Returns whether an account exists in either backend.
    pub fn contains(&self, pubkey: &Pubkey) -> Result<bool> {
        let txn = &mut self.txn.borrow_mut();
        if self.db.persisted.contains(txn, pubkey)? {
            return Ok(true);
        }
        Ok(self.db.volatile.contains(pubkey))
    }
}

/// Internal images consumed only by scoped program reads.
struct ProgramIter<'a> {
    /// Persisted program accounts.
    persisted: Option<PersistedProgramIter<'a>>,
    /// Volatile program pubkeys.
    volatile: BTreeSet<Pubkey>,
    /// Database handle used to resolve volatile accounts.
    db: &'a AccountsDB,
    /// Outlives the persisted iterator and its LMDB transaction.
    _reader: ReadGuard<'a>,
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
    #[error("mapped storage exceeded the maximum mapped size")]
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
