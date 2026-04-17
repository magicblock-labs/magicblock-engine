//! In-memory account cache and program ownership sets.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::BufReader,
    path::Path,
};

use ahash::RandomState;
use scc::HashMap;
use solana_account::{AccountMode, AccountSharedData, DirtyMarkers, OwnedAccount, ReadableAccount};
use solana_pubkey::Pubkey;
use tracing::info;

use crate::{Result, StoreKind, metrics, snapshot::VOLATILE_DB_FILE};

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
        let accounts: AccountsMap = if snapshot.exists() {
            let mut r = BufReader::new(File::open(&snapshot)?);
            let accs: AccountsMap = bincode::deserialize_from(&mut r)?;
            fs::remove_file(snapshot)?;
            info!(
                count = accs.len(),
                "restored volatile accounts from snapshot"
            );
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
    pub(crate) fn upsert<'a, AC>(&self, accounts: AC)
    where
        AC: IntoIterator<Item = &'a (Pubkey, AccountSharedData)>,
    {
        for (pubkey, account) in accounts {
            // An account that has moved to an authoritative mode, or has been
            // closed, drops any stale volatile copy.
            if account.mode().authoritative() || account.is(AccountMode::Closed) {
                self.delete(pubkey);
                continue;
            }
            // Non-authoritative accounts stay volatile and update the program
            // mapping.
            {
                let mut set = self.programs.entry_sync(*account.owner()).or_default();
                BTreeSet::insert(&mut set, *pubkey);
            }
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
        metrics::accounts(StoreKind::Volatile, self.accounts.len() as u64);
    }

    /// Returns the owned account currently cached for `pubkey`.
    pub(crate) fn load(&self, pubkey: &Pubkey) -> Option<OwnedAccount> {
        let entry = self.accounts.get_sync(pubkey)?;
        Some(entry.get().clone())
    }

    /// Returns whether a volatile account exists for `pubkey`.
    pub(crate) fn contains(&self, pubkey: &Pubkey) -> bool {
        self.accounts.contains_sync(pubkey)
    }

    /// Returns the owned accounts currently mapped to `owner`.
    pub(crate) fn program(&self, owner: &Pubkey) -> BTreeSet<Pubkey> {
        self.programs.read_sync(owner, |_, s| s.clone()).unwrap_or_default()
    }

    /// Drops chain-mirrored accounts while retaining internal system accounts.
    pub(crate) fn reset(&self) {
        self.programs.clear_sync();
        self.accounts.retain_sync(|k, a| {
            if !a.is(AccountMode::System) {
                return false;
            }
            let mut set = self.programs.entry_sync(a.owner()).or_default();
            BTreeSet::insert(&mut set, *k)
        });
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
