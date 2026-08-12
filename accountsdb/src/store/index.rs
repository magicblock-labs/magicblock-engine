//! LMDB index for persisted accounts.
//!
//! The index maps compact pubkey tags to storage offsets and owner tags,
//! plus a freelist keyed by image size.

use std::{fs, mem, path::Path};

use heed::{
    Database, DatabaseFlags, Env, EnvFlags, EnvOpenOptions, IntegerComparator, Result, RoIter,
    RoTxn, RwTxn, iteration_method::MoveOnCurrentKeyDuplicates,
};
use nucleus::heed::{DatabaseIndex, RoTxnTls};
use solana_pubkey::Pubkey;

use crate::store::kv::{KeyTail, Offset, OwnerAndOffset, PubkeyBytes, U32LE};

/// LMDB map size for the index database.
#[cfg(feature = "testkit")]
const INDEX_MAP_SIZE: usize = nucleus::MB;
#[cfg(not(feature = "testkit"))]
const INDEX_MAP_SIZE: usize = nucleus::GB;
/// Subdirectory used for the LMDB index.
const INDEX_SUBDIR: &str = "index";
/// Accounts table name.
const ACCOUNTS_INDEX: &str = "accounts";
/// Program ownership table name.
const PROGRAMS_INDEX: &str = "programs";
/// Freelist table name.
const FREELIST_INDEX: &str = "freelist";

/// Iterator over all persisted accounts in pubkey order.
type RoAccountIter<'a> = RoIter<'a, PubkeyBytes, OwnerAndOffset>;
/// Duplicate iterator over program-owned persisted accounts.
type RoProgramIter<'a> = RoIter<'a, KeyTail, Offset, MoveOnCurrentKeyDuplicates>;
/// Iterator over persisted accounts.
pub(crate) struct AccountIter<'a> {
    /// Iterator over `pubkey -> account` entries.
    pub(super) inner: RoAccountIter<'a>,
    /// Keeps the read transaction alive for the iterator lifetime.
    pub(super) _txn: RoTxnTls<'a>,
}
/// Duplicate iterator over persisted accounts for one owner.
pub(crate) struct OwnerIter<'a> {
    /// Duplicates iterator over `owner -> account` entries.
    pub(crate) inner: RoProgramIter<'a>,
    /// Keeps the read transaction alive for the iterator lifetime.
    pub(crate) _txn: RoTxnTls<'a>,
}

/// LMDB index over persisted account offsets and owners.
pub(crate) struct Index {
    /// LMDB environment for the on-disk index.
    pub(super) env: Env,
    /// Account pubkey -> offset + owner keytag.
    pub(super) accounts: Database<PubkeyBytes, OwnerAndOffset>,
    /// Owner keytag -> offset.
    pub(super) programs: Database<KeyTail, Offset>,
    /// Image size -> offset.
    pub(super) freelist: Database<U32LE, Offset, IntegerComparator>,
}

impl Index {
    /// Opens or creates the index directory and databases.
    pub(crate) fn new(path: &Path) -> crate::Result<Self> {
        let path = path.join(INDEX_SUBDIR);
        fs::create_dir_all(&path)?;
        // SAFETY: this process owns the index directory for the lifetime of
        // the database, so the backing files are not mutated behind LMDB's back.
        let env = unsafe {
            EnvOpenOptions::new()
                .max_dbs(3)
                .map_size(INDEX_MAP_SIZE)
                .flags(EnvFlags::WRITE_MAP)
                .flags(EnvFlags::NO_READ_AHEAD)
                .flags(EnvFlags::NO_SYNC)
                .open(path)?
        };

        let mut txn = env.write_txn()?;
        let accounts = env.database_options().name(ACCOUNTS_INDEX).types().create(&mut txn)?;
        let programs = env
            .database_options()
            .name(PROGRAMS_INDEX)
            .flags(DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED)
            .types()
            .create(&mut txn)?;
        let freelist = env
            .database_options()
            .name(FREELIST_INDEX)
            .flags(DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED)
            .key_comparator()
            .types()
            .create(&mut txn)?;
        txn.commit()?;
        Ok(Self {
            env,
            accounts,
            programs,
            freelist,
        })
    }

    /// Returns the persisted offset for `pubkey`.
    pub(crate) fn offset(&self, key: &Pubkey, txn: &RoTxn<'_>) -> Result<Option<Offset>> {
        let entry = self.accounts.get(txn, key)?;
        Ok(entry.map(|e| e.offset))
    }

    /// Takes a freed span from the freelist when one matches `units`.
    pub(crate) fn allocate(&self, units: u32, txn: &mut RwTxn<'_>) -> Result<Option<Offset>> {
        let offset = self.freelist.get(txn, &units)?;
        if let Some(offset) = offset {
            self.freelist.delete_one_duplicate(txn, &units, &offset)?;
            Ok(Some(offset))
        } else {
            Ok(None)
        }
    }

    /// Inserts an account and its owner mapping.
    pub(crate) fn insert(
        &self,
        key: &Pubkey,
        data: OwnerAndOffset,
        txn: &mut RwTxn<'_>,
    ) -> Result<()> {
        self.accounts.put(txn, key, &data)?;
        let OwnerAndOffset { owner, offset } = data;
        self.programs.put(txn, &owner, &offset)
    }

    /// Removes an account and returns its persisted offset.
    pub(crate) fn delete(&self, key: &Pubkey, txn: &mut RwTxn<'_>) -> Result<Option<Offset>> {
        let Some(entry) = self.accounts.get(txn, key)? else {
            return Ok(None);
        };

        let OwnerAndOffset { owner, offset } = entry;
        self.accounts.delete(txn, key)?;

        self.programs.delete_one_duplicate(txn, &owner, &offset)?;
        Ok(Some(offset))
    }

    /// Returns the duplicate iterator for accounts owned by `owner`.
    pub(crate) fn program<'a>(&'a self, owner: Pubkey) -> Result<Option<OwnerIter<'a>>> {
        let owner = owner.into();
        let txn = self.env.read_txn()?;
        let Some(iter) = self.programs.get_duplicates(&txn, &owner)? else {
            return Ok(None);
        };
        // The duplicate iterator borrows `txn`; storing it in the wrapper keeps
        // the borrow alive for the iterator lifetime.
        // SAFETY: the wrapper owns `txn`, so the duplicate iterator cannot outlive it.
        let iter = unsafe { mem::transmute::<RoProgramIter<'_>, RoProgramIter<'a>>(iter) };
        Ok(Some(OwnerIter { _txn: txn, inner: iter }))
    }

    /// Returns an iterator over all accounts in pubkey order.
    pub(crate) fn accounts<'a>(&'a self) -> Result<AccountIter<'a>> {
        let txn = self.env.read_txn()?;
        let iter = self.accounts.iter(&txn)?;
        // The iterator borrows `txn`; storing it in the wrapper keeps the
        // transaction alive for the iterator lifetime.
        // SAFETY: the wrapper owns `txn`, so the iterator cannot outlive it.
        let iter = unsafe { mem::transmute::<RoAccountIter<'_>, RoAccountIter<'a>>(iter) };
        Ok(AccountIter { _txn: txn, inner: iter })
    }

    /// Moves an account entry to a new owner while preserving its offset.
    pub(crate) fn update_owner(
        &self,
        acc: &Pubkey,
        new: KeyTail,
        txn: &mut RwTxn<'_>,
    ) -> Result<()> {
        let Some(val) = self.accounts.get(txn, acc)? else {
            return Ok(());
        };
        let OwnerAndOffset { owner: old, offset } = val;
        self.programs.delete_one_duplicate(txn, &old, &offset)?;

        let data = OwnerAndOffset { owner: new, offset };
        self.accounts.put(txn, acc, &data)?;
        self.programs.put(txn, &new, &offset)
    }

    /// Moves an account entry to a new offset while preserving its owner.
    pub(crate) fn relocate(
        &self,
        key: &Pubkey,
        old: Offset,
        new: OwnerAndOffset,
        txn: &mut RwTxn<'_>,
    ) -> Result<()> {
        self.accounts.put(txn, key, &new)?;
        let OwnerAndOffset { owner, offset } = new;
        self.programs.delete_one_duplicate(txn, &owner, &old)?;
        self.programs.put(txn, &owner, &offset)
    }
}

impl DatabaseIndex for Index {
    fn env(&self) -> &Env {
        &self.env
    }
}
