//! Read-side caches owned by keeper.

use std::{collections::VecDeque, hash::Hash, sync::Arc};

use ahash::RandomState;
use arc_swap::ArcSwap;
use ledger::request::{BlockHistory, TransactionStatus};
use nucleus::{Slot, ledger::Block, notifier::EventNotifier};
use parking_lot::Mutex;
use scc::{HashCache, HashMap, hash_map::Entry};
use solana_account::AccountMode;
use solana_hash::Hash as SolanaHash;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_sysvar::slot_hashes::SlotHashes;

use crate::{
    metrics,
    subscriptions::{Subscription, Unicast},
};

/// Block and transaction history captured at one authoritative startup boundary.
pub(crate) struct CacheSeed {
    latest: Block,
    hashes: Vec<(Slot, SolanaHash)>,
    history: BlockHistory,
}

impl CacheSeed {
    /// Reconciles ledger history with persisted SlotHashes at one state boundary.
    pub(crate) fn new(history: BlockHistory, slothashes: Option<&SlotHashes>) -> Self {
        let Some(slothashes) = slothashes.map(SlotHashes::slot_hashes) else {
            let latest = history.first().map_or(Block::default(), |entry| entry.block);
            let hashes =
                history.iter().rev().map(|entry| (entry.block.slot, entry.block.hash)).collect();
            return Self { latest, hashes, history };
        };

        let mut hashes = slothashes.to_vec();
        hashes.extend(
            history
                .iter()
                .skip(hashes.len())
                .map(|entry| (entry.block.slot, entry.block.hash)),
        );
        let latest = hashes.first().map_or(Block::default(), |&(slot, hash)| Block {
            slot,
            hash,
            time: history.first().map_or(0, |entry| entry.block.time),
            parent: hashes.get(1).map(|(_, hash)| *hash).unwrap_or_default(),
        });
        hashes.reverse();
        Self { latest, hashes, history }
    }

    /// Returns the latest authoritative block boundary.
    pub(crate) fn latest(&self) -> Block {
        self.latest
    }
}

pub(crate) struct Caches {
    /// Recent signature statuses keyed by transaction signature.
    pub(crate) signatures: ExpiringCache<Signature, Option<TransactionStatus>>,
    /// Recent block hashes and latest block boundary.
    pub(crate) blocks: BlocksCache,
    /// Account recency and missing-load coordination.
    pub(crate) accounts: Arc<AccountCache>,
}

impl Caches {
    /// Creates read caches from one authoritative startup history.
    pub(crate) fn new(
        seed: CacheSeed,
        block_ttl: Slot,
        signature_ttl: Slot,
        account_capacity: usize,
    ) -> Self {
        let CacheSeed { latest, hashes, history } = seed;
        let caches = Self {
            blocks: BlocksCache::new(latest, hashes, block_ttl),
            signatures: ExpiringCache::new(signature_ttl),
            accounts: Arc::new(AccountCache::new(account_capacity)),
        };
        for (slot, status) in history.into_iter().rev().flat_map(|block| {
            let slot = block.block.slot;
            block.signatures.into_iter().map(move |status| (slot, status))
        }) {
            caches.signatures.push(status.signature, Some(status.status), slot);
        }
        caches
    }
}

/// Account access cache along with missing-account load reservations.
pub(crate) struct AccountCache {
    /// Committed account loads, ordered by recent access.
    pub(crate) lru: HashCache<Pubkey, (), RandomState>,
    /// In-flight account loads keyed by account pubkey.
    pub(crate) reservations: HashMap<Pubkey, Arc<EventNotifier>, RandomState>,
    /// Pubkeys evicted when a committed load displaces a cold account.
    pub(crate) evictions: Unicast<Pubkey>,
}

impl AccountCache {
    /// Creates missing-load coordination with the requested recent-load capacity.
    pub(crate) fn new(capacity: usize) -> Self {
        let lru = HashCache::with_capacity_and_hasher(256, capacity, Default::default());
        Self {
            lru,
            reservations: Default::default(),
            evictions: Unicast::new(32, Subscription::Evictions),
        }
    }
}

/// Load guard owned by the task responsible for one missing account.
pub struct AccountLoad {
    /// Account this guard is responsible for loading.
    pub pubkey: Pubkey,
    /// Cache reservation released on commit or drop.
    cache: Option<Arc<AccountCache>>,
}

/// Wait handle for an account currently being loaded by another task.
pub struct AccountWait(Pubkey, Arc<EventNotifier>);

/// Coordination state for one account missing from local storage.
pub enum MissingAccount {
    /// Caller owns the load; dropping the guard releases waiters.
    Load(AccountLoad),
    /// Another caller is already loading the account.
    Wait(AccountWait),
}

impl AccountLoad {
    /// Completes the load, wakes waiters, and tracks non-authoritative modes for
    /// eviction.
    pub async fn complete(mut self, mode: AccountMode) {
        let Some((cache, notifier)) = self.release() else {
            return;
        };
        notifier.notify(true);
        if mode.authoritative() {
            return;
        }
        if let Ok(Some(evicted)) = cache.lru.put_sync(self.pubkey, ()) {
            metrics::account_cache_eviction();
            cache.evictions.send(evicted.0).await;
        } else {
            metrics::account_cache_insert()
        }
    }

    fn release(&mut self) -> Option<(Arc<AccountCache>, Arc<EventNotifier>)> {
        let cache = self.cache.take()?;
        let (_, notifier) = cache.reservations.remove_sync(&self.pubkey)?;
        Some((cache, notifier))
    }
}

impl Drop for AccountLoad {
    /// Cancels the load reservation and wakes waiters without caching.
    fn drop(&mut self) {
        let Some((_, notifier)) = self.release() else {
            return;
        };
        notifier.notify(false);
    }
}

impl AccountWait {
    /// Waits for the active loader and returns the pubkey plus whether it committed.
    pub async fn wait(self) -> (Pubkey, bool) {
        let result = self.1.notified().await;
        (self.0, result)
    }
}

impl AccountCache {
    /// Promotes `pubkey` only if it has already been committed to the cache.
    pub(crate) fn promote(&self, pubkey: &Pubkey) {
        self.lru.get_sync(pubkey);
    }

    /// Reserves a missing account load or returns a waiter for the active load.
    pub(crate) fn reserve(self: &Arc<Self>, pubkey: Pubkey) -> MissingAccount {
        match self.reservations.entry_sync(pubkey) {
            Entry::Occupied(e) => {
                metrics::account_resolution_race();
                MissingAccount::Wait(AccountWait(pubkey, e.get().clone()))
            }
            Entry::Vacant(e) => {
                let notifier = Arc::new(EventNotifier::default());
                e.insert_entry(notifier);
                MissingAccount::Load(AccountLoad {
                    pubkey,
                    cache: Some(self.clone()),
                })
            }
        }
    }
}

/// Block lookup cache with a lock-free latest-block pointer.
pub(crate) struct BlocksCache {
    /// Latest block boundary known at keeper startup or after updates.
    pub(crate) latest: ArcSwap<Block>,
    /// Recent block hash to slot lookups.
    pub(crate) history: ExpiringCache<SolanaHash, Slot>,
}

impl BlocksCache {
    /// Creates a block cache seeded with retained history in ascending slot order.
    pub(crate) fn new(latest: Block, history: Vec<(Slot, SolanaHash)>, ttl: Slot) -> Self {
        let cache = Self {
            latest: ArcSwap::new(latest.into()),
            history: ExpiringCache::new(ttl),
        };
        if history.is_empty() {
            cache.history.push(latest.hash, latest.slot, latest.slot);
        } else {
            for (slot, hash) in history {
                cache.history.push(hash, slot, slot);
            }
        }
        cache
    }

    /// Records `block` as the latest and adds its hash to the recent history.
    pub(crate) fn push(&self, block: Block) {
        self.latest.store(block.into());
        self.history.push(block.hash, block.slot, block.slot);
        metrics::block_hash_entries(self.history.len());
    }
}

/// Concurrent cache with slot-based lazy eviction.
///
/// Entries are evicted only when another entry is pushed. Re-inserting an
/// existing key leaves its value and expiry slot unchanged.
pub(crate) struct ExpiringCache<K, V> {
    /// Cached values by key.
    index: HashMap<K, V>,
    /// Expiry order used for lazy eviction.
    queue: Mutex<VecDeque<ExpiringRecord<K>>>,
    /// Number of slots each entry lives after insertion.
    ttl: Slot,
}

struct ExpiringRecord<K> {
    key: K,
    expires: Slot,
}

impl<K: Hash + Eq + Copy + 'static, V: Clone> ExpiringCache<K, V> {
    /// Creates a cache whose entries live for `ttl` slots after insertion.
    pub(crate) fn new(ttl: Slot) -> Self {
        Self {
            index: HashMap::default(),
            queue: Default::default(),
            ttl,
        }
    }

    /// Insert a key and evict entries expired at `slot`.
    ///
    /// Returns `false` if `key` already exists. Existing values and expiry slots
    /// are left unchanged.
    pub(crate) fn push(&self, key: K, value: V, slot: Slot) -> bool {
        let mut queue = self.queue.lock();
        // Lazily evict expired entries from the front of the queue.
        while let Some(expired) = queue.pop_front_if(|e| e.expired(slot)) {
            self.index.remove_sync(&expired.key);
        }

        match self.index.entry_sync(key) {
            Entry::Occupied(_) => false,
            Entry::Vacant(v) => {
                v.insert_entry(value);
                queue.push_back(ExpiringRecord::new(key, slot + self.ttl));
                true
            }
        }
    }

    /// Entry count, including expired entries no sweep has reached yet, so the
    /// metrics gauges fed from this can read above the live entry count.
    pub(crate) fn len(&self) -> usize {
        self.index.len()
    }

    /// May yield an entry already past its expiry slot but not yet swept.
    pub(crate) fn get(&self, key: &K) -> Option<V> {
        self.index.read_sync(key, |_, v| v.clone())
    }

    /// May report an entry already past its expiry slot but not yet swept.
    pub(crate) fn contains(&self, key: &K) -> bool {
        self.index.contains_sync(key)
    }

    /// Replaces the value for `key`, leaving its expiry slot untouched: an
    /// update does not extend the entry's life. No-op if `key` is absent.
    pub(crate) fn update(&self, key: &K, value: V) {
        let Some(mut entry) = self.index.get_sync(key) else {
            return;
        };
        entry.insert(value);
    }
}

impl<K> ExpiringRecord<K> {
    fn new(key: K, expires: Slot) -> Self {
        Self { key, expires }
    }

    fn expired(&self, instant: Slot) -> bool {
        instant >= self.expires
    }
}
