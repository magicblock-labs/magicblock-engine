//! In-memory account cache and program ownership sets.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::BufReader,
    path::Path,
};

use ahash::RandomState;
use scc::HashMap;
use solana_account::{AccountSharedData, DirtyMarkers, OwnedAccount, ReadableAccount};
use solana_pubkey::Pubkey;

use crate::{
    Result,
    snapshot::{SnapshotError, VOLATILE_DB_FILE},
};

/// Owned accounts keyed by account pubkey.
type AccountsMap = HashMap<Pubkey, OwnedAccount, RandomState>;
/// Program ownership sets keyed by owner pubkey.
type ProgramsMap = HashMap<Pubkey, BTreeSet<Pubkey>, RandomState>;

/// Volatile account store backed by concurrent hash maps.
pub(crate) struct VolatileStore {
    /// Current owned accounts.
    pub(crate) accounts: AccountsMap,
    /// Program owner -> account pubkeys.
    pub(crate) programs: ProgramsMap,
}

impl VolatileStore {
    /// Opens the volatile store, optionally bootstrapping from a snapshot file.
    ///
    /// If `volatile.db` exists, it is loaded into memory and then removed from
    /// the snapshot directory so the active tree stays single-sourced.
    pub(crate) fn new(path: &Path) -> Result<Self> {
        const CAP: usize = 2048;
        let snapshot = path.join(VOLATILE_DB_FILE);
        let accounts = if snapshot.exists() {
            let mut r = BufReader::new(File::open(&snapshot)?);
            let accs = bincode::deserialize_from(&mut r).map_err(SnapshotError::from)?;
            fs::remove_file(snapshot)?;
            accs
        } else {
            AccountsMap::with_capacity_and_hasher(CAP, Default::default())
        };

        let programs = ProgramsMap::with_capacity_and_hasher(CAP, Default::default());
        accounts.iter_sync(|&pk, acc| {
            BTreeSet::insert(&mut programs.entry_sync(acc.owner()).or_default(), pk)
        });
        Ok(Self { accounts, programs })
    }

    /// Stores volatile accounts and keeps the program ownership sets in sync.
    pub(crate) fn upsert<'a, AI>(&self, accounts: AI)
    where
        AI: IntoIterator<Item = &'a (Pubkey, AccountSharedData)>,
    {
        for (pubkey, account) in accounts {
            // Mutable accounts live in persisted storage,
            // so here we remove any stale volatile copy.
            if account.mutable() {
                self.delete(pubkey);
                continue;
            }
            // Immutable accounts stay volatile and update the program mapping.
            let mut set = self.programs.entry_sync(*account.owner()).or_default();
            BTreeSet::insert(&mut set, *pubkey);
            let Some(prev) = self.accounts.upsert_sync(*pubkey, account.owned()) else {
                continue;
            };
            if !account.markers().contains(DirtyMarkers::OWNER) {
                continue;
            }
            // Only the old owner set needs cleanup; the new owner was inserted above.
            self.programs.remove_if_sync(&prev.owner(), |set| {
                set.remove(pubkey);
                set.is_empty()
            });
        }
    }

    /// Returns the owned account currently cached for `pubkey`.
    pub(crate) fn load(&self, pubkey: &Pubkey) -> Option<OwnedAccount> {
        let entry = self.accounts.get_sync(pubkey)?;
        Some(entry.get().clone())
    }

    /// Returns the owned accounts currently mapped to `owner`.
    pub(crate) fn program(&self, owner: &Pubkey) -> BTreeSet<Pubkey> {
        self.programs.read_sync(owner, |_, s| s.clone()).unwrap_or_default()
    }

    /// Removes the cached account and drops its owner mapping.
    fn delete(&self, pubkey: &Pubkey) {
        let Some(e) = self.accounts.remove_sync(pubkey) else { return };
        self.programs.remove_if_sync(&e.1.owner(), |set| {
            set.remove(pubkey);
            set.is_empty()
        });
    }
}
