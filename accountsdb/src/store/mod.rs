//! Persisted account load and write path.
//!
//! This module coordinates the mmap, LMDB index, and borrowed account layout.

use core::slice;

use solana_account::{
    AccountSharedData, BorrowedAccount, CoWAccount::*, DirtyMarkers, OwnedAccount,
};
use solana_pubkey::Pubkey;

use crate::{
    AccountEntry, Result,
    store::{
        index::{Index, OptRoTxn, OptRwTxn, OwnerIter},
        kv::OwnerAndOffset,
        mmap::MappedStorage,
    },
};

mod defrag;
pub(crate) mod index;
mod kv;
mod mmap;

/// Current on-disk storage format version.
pub(crate) const VERSION: DatabaseVersion = 1;
/// Version tag stored in the metadata header.
pub(crate) type DatabaseVersion = u64;

/// Borrowed arguments passed into persisted write helpers.
///
/// The optional transaction is threaded through the batch so index and mmap
/// changes stay in one write path.
type WriteArgs<'t, 'e, A> = (&'t Pubkey, &'t A, &'t DirtyMarkers, OptRwTxn<'t, 'e>);

/// Persisted store backed by the mmap and LMDB index.
pub(crate) struct PersistedStore {
    /// Mapped account storage.
    pub(crate) storage: MappedStorage,
    /// LMDB index over persisted accounts.
    pub(crate) index: Index,
}

/// Iterator over persisted program-owned accounts.
pub(crate) struct PersistedProgramIter<'a> {
    /// Keeps the read transaction alive while iterating.
    iter: OwnerIter<'a>,
    /// Mapped storage backing the returned borrowed accounts.
    mmap: &'a MappedStorage,
}

impl PersistedStore {
    /// Opens or creates the persisted store at `path`.
    pub(crate) fn new(path: &std::path::Path) -> Result<Self> {
        let index = Index::new(path)?;
        let storage = MappedStorage::new(path)?;
        storage.validate()?;
        Ok(Self { storage, index })
    }

    /// Loads the persisted image for `pubkey` from the mapped file.
    pub(crate) fn load<'e>(
        &'e self,
        txn: OptRoTxn<'_, 'e>,
        pubkey: &Pubkey,
    ) -> Result<Option<BorrowedAccount>> {
        let txn = self.index.unwrap_ro(txn)?;
        let offset = self.index.offset(pubkey, txn)?;
        offset.is_some().then(|| self.storage.stats().read());
        // SAFETY: offsets come from the persisted index and point into the map.
        Ok(offset.map(|o| unsafe { BorrowedAccount::init(self.storage.at(o)) }))
    }

    /// Applies a batch of account updates to the persisted store.
    ///
    /// Borrowed mutable accounts are committed in place. Owned mutable accounts
    /// are serialized into the mmap. Immutable accounts delete stale persisted
    /// entries. If the LMDB commit fails or database runs out of space, the borrowed
    /// images are rolled back so in-memory state stays aligned with the durable index.
    pub(crate) fn upsert<'a, AI>(&self, accounts: AI) -> Result<()>
    where
        AI: IntoIterator<Item = &'a AccountEntry> + Clone,
    {
        let mut applied = 0;
        let mut result = Ok(());
        let mut txn = None;
        for entry in accounts.clone() {
            result = self.apply(entry, &mut txn);
            if result.is_err() {
                break;
            }
            applied += 1;
        }
        // Commit once after the batch so the index and mmap stay in sync.
        if let Some(txn) = txn
            && result.is_ok()
        {
            result = txn.commit().map_err(Into::into);
        }
        if result.is_err() {
            // Only borrowed accounts need rollback here: owned inserts never
            // mutate an existing borrowed image in place.
            let processed = accounts.into_iter().take(applied).map(|(_, a)| a);
            Self::rollback(processed);
        }
        result
    }

    /// Returns the persisted program iterator for `owner`.
    pub(crate) fn program(&self, owner: Pubkey) -> Result<Option<PersistedProgramIter<'_>>> {
        let i = self.index.program(owner)?;
        Ok(i.map(|iter| PersistedProgramIter { iter, mmap: &self.storage }))
    }

    /// Flushes the mapped storage and LMDB index to durable storage.
    pub(crate) fn flush(&self, sync: bool) -> heed::Result<()> {
        self.storage.flush(sync)?;
        self.index.flush()
    }

    /// Applies one account state transition to the persisted backend.
    fn apply<'e>(&'e self, acc: &AccountEntry, txn: OptRwTxn<'_, 'e>) -> Result<()> {
        let (pubkey, account) = acc;
        // Immutable accounts are not authoritative here,
        // so remove any stale persisted entry.
        if !account.mutable() {
            return self.delete(pubkey, txn);
        }

        let markers = account.markers();
        match account.cow() {
            Borrowed(acc) => self.update((pubkey, acc, markers, txn)),
            Owned(acc) => self.insert((pubkey, acc, markers, txn)),
        }
    }

    /// Rolls back borrowed images that were already touched in the batch.
    fn rollback<'a, AI>(accounts: AI)
    where
        AI: Iterator<Item = &'a AccountSharedData>,
    {
        for acc in accounts {
            let Borrowed(acc) = acc.cow() else { continue };
            // SAFETY: only borrowed accounts were updated before the failed commit.
            unsafe { acc.rollback() };
        }
    }

    /// Commits a borrowed image after updating its owner mapping if needed.
    fn update<'e>(&'e self, args: WriteArgs<'_, 'e, BorrowedAccount>) -> Result<()> {
        let (pubkey, acc, markers, txn) = args;
        if markers.contains(DirtyMarkers::OWNER) {
            let txn = self.index.unwrap_rw(txn)?;
            let owner = acc.owner().into();
            self.index.update_owner(pubkey, owner, txn)?;
        }
        acc.commit();
        self.storage.stats().commit();
        Ok(())
    }

    /// Serializes an owned image into mapped storage and records its offset.
    fn insert<'e>(&'e self, args: WriteArgs<'_, 'e, OwnedAccount>) -> Result<()> {
        let (pubkey, acc, markers, txn) = args;
        let txn = self.index.unwrap_rw(txn)?;
        let units = acc.units();
        let owner = acc.owner().into();
        if markers.contains(DirtyMarkers::OWNER) {
            self.index.update_owner(pubkey, owner, txn)?;
        }

        let (ptr, offset) = if let Some(offset) = self.index.allocate(units, txn)? {
            let ptr = self.storage.at(offset);
            self.storage.stats().reallocs();
            (ptr, offset)
        } else {
            let alloc = self.storage.allocate(units)?;
            (alloc.ptr, alloc.offset)
        };
        let data = OwnerAndOffset { owner, offset };
        self.index.insert(pubkey, data, txn)?;
        // SAFETY: `ptr` points at a fresh span inside the mapped storage and
        // `units` is the exact serialized size of this owned account.
        unsafe {
            let buffer = slice::from_raw_parts_mut(ptr.as_ptr(), units as usize);
            acc.serialize(buffer, pubkey);
        };
        Ok(())
    }

    /// Removes a persisted image and returns its storage span to the freelist.
    fn delete<'e>(&'e self, pubkey: &Pubkey, txn: OptRwTxn<'_, 'e>) -> Result<()> {
        let txn = self.index.unwrap_rw(txn)?;
        let Some(offset) = self.index.delete(pubkey, txn)? else {
            return Ok(());
        };
        // SAFETY: `offset` was returned by the index and still points at a valid image.
        let space = unsafe { BorrowedAccount::span(self.storage.at(offset)) };
        self.index.freelist.put(txn, &space, &offset)?;
        self.storage.stats().remove();
        Ok(())
    }
}

impl<'a> Iterator for PersistedProgramIter<'a> {
    type Item = AccountEntry;

    fn next(&mut self) -> Option<Self::Item> {
        let (_, offset) = self.iter.inner.next()?.ok()?;
        let ptr = self.mmap.at(offset);
        // The image prefix stores the full pubkey, so iteration can recover it
        // without consulting LMDB again.
        // SAFETY: the iterator yields offsets stored in the same mapped database.
        self.mmap.stats().read();
        let pubkey = unsafe { BorrowedAccount::pubkey(ptr) };
        let account = unsafe { BorrowedAccount::init(ptr).into() };
        Some((pubkey, account))
    }
}
