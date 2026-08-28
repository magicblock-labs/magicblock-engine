//! Persisted account load and write path.
//!
//! This module coordinates the mmap, LMDB index, and borrowed account layout.

use core::{hash::Hasher, slice};
use std::sync::atomic::Ordering::{Acquire, Release};

use solana_account::{
    AccountMode, AccountSharedData, BorrowedAccount, CoWAccount::*, DirtyMarkers, OwnedAccount,
    StorageUnit,
};
use solana_pubkey::Pubkey;
use tracing::{error, warn};

use twox_hash::XxHash3_64;

use crate::{
    AccountEntry, AccountsDBError, Result, StoreKind,
    metrics::{self, Operation},
    store::{
        index::{Index, OptRoTxn, OptRwTxn, OwnerIter, read_txn, write_txn},
        kv::{KeyTail, Offset, OwnerAndOffset},
        mmap::{DatabaseMeta, MappedStorage},
    },
};

mod defrag;
pub(crate) mod index;
mod kv;
pub(crate) mod mmap;

#[cfg(test)]
pub(crate) use defrag::MIN_REMAINDER;
pub(crate) use mmap::Stats;

/// Current on-disk storage format version.
pub(crate) const VERSION: DatabaseVersion = 1;
/// Version tag stored in the metadata header.
pub(crate) type DatabaseVersion = u64;

/// Snapshot of an in-place owned overwrite that is not yet durable.
struct ReusedOverwrite {
    /// Live image offset that was overwritten.
    offset: Offset,
    /// Full existing span, including headers, taken before serialize.
    snapshot: Vec<StorageUnit>,
    /// Account whose owner mapping may have been updated in the open txn.
    pubkey: Pubkey,
    /// Owner tag recorded in the index before this overwrite.
    owner: KeyTail,
}

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
        Ok(Self { storage, index })
    }

    /// Loads the persisted image for `pubkey` from the mapped file.
    pub(crate) fn load<'e>(
        &'e self,
        txn: OptRoTxn<'_, 'e>,
        pubkey: &Pubkey,
    ) -> Result<Option<BorrowedAccount>> {
        let txn = read_txn(self.index.env(), txn)?;
        let offset = self.index.offset(pubkey, txn)?;
        offset.is_some().then(|| self.storage.stats().read());
        // SAFETY: offsets come from the persisted index and point into the map.
        Ok(offset.map(|o| unsafe { BorrowedAccount::init(self.storage.at(o)) }))
    }

    /// Returns whether a persisted account image exists for `pubkey`.
    pub(crate) fn contains<'e>(&'e self, txn: OptRoTxn<'_, 'e>, pubkey: &Pubkey) -> Result<bool> {
        let txn = read_txn(self.index.env(), txn)?;
        self.index.offset(pubkey, txn).map(|o| o.is_some()).map_err(Into::into)
    }

    /// Applies a batch of account updates to the persisted store.
    ///
    /// Borrowed accounts in authoritative modes are committed in place. Owned
    /// accounts in those modes are serialized into the mmap. Other modes delete
    /// stale persisted entries. If a later apply or the LMDB commit fails, borrowed
    /// images and in-place owned reuse are rolled back so mmap state stays
    /// aligned with the durable index.
    pub(crate) fn upsert<'a, AC>(&self, accounts: AC) -> Result<()>
    where
        AC: IntoIterator<Item = &'a AccountEntry> + Clone,
    {
        let mut applied = 0;
        let mut result = Ok(());
        let mut txn = None;
        let mut reused = Vec::new();
        for entry in accounts.clone() {
            result = self.apply(entry, &mut txn, &mut reused);
            if result.is_err() {
                break;
            }
            applied += 1;
        }
        // Commit once after the batch so the index and mmap stay in sync.
        if result.is_ok()
            && let Some(txn) = txn.take()
        {
            match self.index.accounts.len(&txn) {
                Ok(count) => {
                    metrics::accounts(StoreKind::Persisted, count);
                    result = txn.commit().map_err(Into::into);
                }
                Err(error) => result = Err(error.into()),
            }
        }
        if let Err(error) = &result {
            warn!(applied, ?error, "accounts persistence failed; rolling back");
            // Borrowed commits and in-place owned reuse write the mmap before
            // the LMDB commit. Borrowed views undo via sequence; reused owned
            // images restore the snapshot taken before serialize.
            let processed = accounts.into_iter().take(applied).map(|(_, a)| a);
            Self::rollback(processed);
            self.restore_reused(&reused, txn.as_mut());
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
        let _timer = metrics::time(Operation::Flush);
        self.index.flush()?;
        if sync {
            let checksum = self.checksum()?;
            self.meta().checksum.store(checksum, Release);
        }
        self.storage.flush(sync)?;
        Ok(())
    }

    /// Validates the persisted store checksum and on-disk format version.
    pub(crate) fn validate(&self) -> Result<()> {
        self.storage.validate()?;
        if self.storage.cursor() == 0 {
            return Ok(());
        }
        let expected = self.meta().checksum.load(Acquire);
        let actual = self.checksum()?;
        if expected != actual {
            error!(expected, actual, "state checksum mismatch");
            return Err(AccountsDBError::Corruption);
        }

        Ok(())
    }

    /// Applies one account state transition to the persisted backend.
    fn apply<'e>(
        &'e self,
        acc: &AccountEntry,
        txn: OptRwTxn<'_, 'e>,
        reused: &mut Vec<ReusedOverwrite>,
    ) -> Result<()> {
        let (pubkey, account) = acc;
        // An account that has moved to a non-authoritative mode, or has been
        // closed, no longer belongs here, so drop any stale persisted entry.
        if !account.mode().authoritative() || account.is(AccountMode::Closed) {
            self.delete(pubkey, txn)?;
            if let Borrowed(acc) = account.cow() {
                acc.commit();
            }
            return Ok(());
        }

        let markers = account.markers();
        match account.cow() {
            Borrowed(acc) => self.update(pubkey, acc, markers, txn),
            Owned(acc) => self.insert(pubkey, acc, txn, reused),
        }
    }

    /// Rolls back borrowed accounts that were already touched in the batch.
    fn rollback<'a, AC>(accounts: AC)
    where
        AC: Iterator<Item = &'a AccountSharedData>,
    {
        for acc in accounts {
            if !acc.dirty() {
                continue;
            }
            let Borrowed(acc) = acc.cow() else { continue };
            // SAFETY: only borrowed accounts were updated before the failed commit.
            unsafe { acc.rollback() };
        }
    }

    /// Restores overwritten owned spans and their prior owner mappings.
    fn restore_reused(&self, reused: &[ReusedOverwrite], txn: Option<&mut heed::RwTxn<'_>>) {
        for image in reused {
            let ptr = self.storage.at(image.offset);
            // SAFETY: `offset` is the live span we overwrote; `snapshot` is that
            // span including headers, taken before serialize.
            unsafe {
                let dest = slice::from_raw_parts_mut(ptr.as_ptr(), image.snapshot.len());
                dest.copy_from_slice(&image.snapshot);
            }
        }
        let Some(txn) = txn else {
            return;
        };
        for image in reused {
            let _ = self.index.update_owner(&image.pubkey, image.owner, txn);
        }
    }

    /// Commits a borrowed image after updating its owner mapping if needed.
    fn update<'e>(
        &'e self,
        pubkey: &Pubkey,
        acc: &BorrowedAccount,
        markers: &DirtyMarkers,
        txn: OptRwTxn<'_, 'e>,
    ) -> Result<()> {
        if markers.contains(DirtyMarkers::OWNER) {
            let txn = write_txn(self.index.env(), txn)?;
            let owner = acc.owner().into();
            self.index.update_owner(pubkey, owner, txn)?;
        }
        if !markers.intersects(DirtyMarkers::all()) {
            return Ok(());
        }
        acc.commit();
        self.storage.stats().commit();
        Ok(())
    }

    /// Serializes an owned image into mapped storage and records its offset.
    ///
    /// Reuses the live slot when the new payload still fits. Otherwise
    /// allocates a page-aligned span so small resizes do not append a copy.
    fn insert<'e>(
        &'e self,
        pubkey: &Pubkey,
        acc: &OwnedAccount,
        txn: OptRwTxn<'_, 'e>,
        reused: &mut Vec<ReusedOverwrite>,
    ) -> Result<()> {
        let txn = write_txn(self.index.env(), txn)?;
        let owner = acc.owner().into();
        let live = self.index.accounts.get(txn, pubkey)?;
        let existing = match live {
            // SAFETY: live offsets come from the persisted index.
            Some(data) => unsafe { BorrowedAccount::span(self.storage.at(data.offset)) },
            None => 0,
        };
        let units = acc.units_at_least(existing);
        if let Some(prior) = live
            && existing >= acc.units()
        {
            let ptr = self.storage.at(prior.offset);
            // SAFETY: `prior.offset` is the live image; `existing` is its full span.
            let snapshot =
                unsafe { slice::from_raw_parts(ptr.as_ptr(), existing as usize) }.to_vec();
            self.index.update_owner(pubkey, owner, txn)?;
            // SAFETY: `offset` is the live image and `existing` still fits.
            unsafe {
                let buffer = slice::from_raw_parts_mut(ptr.as_ptr(), existing as usize);
                acc.serialize(buffer, pubkey);
            }
            reused.push(ReusedOverwrite {
                offset: prior.offset,
                snapshot,
                pubkey: *pubkey,
                owner: prior.owner,
            });
            return Ok(());
        }

        let (ptr, offset) = if let Some(offset) = self.index.allocate(units, txn)? {
            let ptr = self.storage.at(offset);
            self.storage.stats().realloc();
            (ptr, offset)
        } else {
            let alloc = self.storage.allocate(units)?;
            (alloc.ptr, alloc.offset)
        };
        let data = OwnerAndOffset { owner, offset };
        if let Some(offset) = self.index.delete(pubkey, txn)? {
            self.free(offset, txn)?;
        }
        self.index.insert(pubkey, data, txn)?;
        // SAFETY: `ptr` points at a fresh span inside the mapped storage and
        // `units` is the exact serialized size of this owned account.
        unsafe {
            let buffer = slice::from_raw_parts_mut(ptr.as_ptr(), units as usize);
            acc.serialize(buffer, pubkey);
        };
        Ok(())
    }

    /// Returns one persisted span to the freelist.
    fn free(&self, offset: Offset, txn: &mut heed::RwTxn<'_>) -> Result<()> {
        // SAFETY: `offset` was returned by the index and still points at a valid image.
        let space = unsafe { BorrowedAccount::span(self.storage.at(offset)) };
        self.index.freelist.put(txn, &space, &offset)?;
        Ok(())
    }

    /// Removes a persisted image and returns its storage span to the freelist.
    fn delete<'e>(&'e self, pubkey: &Pubkey, txn: OptRwTxn<'_, 'e>) -> Result<()> {
        let txn = write_txn(self.index.env(), txn)?;
        let Some(offset) = self.index.delete(pubkey, txn)? else {
            return Ok(());
        };
        self.free(offset, txn)?;
        self.storage.stats().remove();
        Ok(())
    }

    /// Returns the persisted storage metadata header.
    pub(crate) fn meta(&self) -> &DatabaseMeta {
        self.storage.meta()
    }

    /// Computes a deterministic checksum over persisted accounts in pubkey order.
    fn checksum(&self) -> heed::Result<u64> {
        let _timer = metrics::time(Operation::Checksum);
        let mut hasher = XxHash3_64::new();
        let mut iter = self.index.accounts()?;
        hasher.write(&self.meta().slot.load(Acquire).to_le_bytes());
        hasher.write(&self.meta().superblock.load(Acquire).to_le_bytes());
        for entry in &mut iter.inner {
            let (pubkey, data) = entry?;
            hasher.write(pubkey.as_array());
            // SAFETY: offsets come from the persisted accounts index and point
            // into the mapped storage for this store.
            let account = unsafe { BorrowedAccount::init(self.storage.at(data.offset)) };
            hasher.write(account.storage());
        }
        Ok(hasher.finish())
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
