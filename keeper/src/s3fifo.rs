//! Lock-free S3-FIFO residency cache.
//!
//! S3-FIFO (Yang et al., SOSP '23) keeps three FIFO structures:
//!
//! - **small**: probation for newly admitted keys, about 10% of capacity;
//! - **main**: keys that were hit while on probation, or readmitted from ghost;
//! - **ghost**: fingerprints of keys recently evicted from small.
//!
//! A hit only bumps a saturating 2-bit counter on the entry, so the read path
//! is one lock-free index lookup plus at most one atomic compare-exchange. Queue
//! order changes only on admission and eviction. Keys seen once leave from
//! small without displacing frequently hit keys in main, which makes the cache
//! resistant to bursts of one-off keys.
//!
//! Concurrency: keys are spread by hash over independent shards so admissions
//! and evictions on different shards never touch the same queue. Each shard
//! indexes entries in an [`scc::HashIndex`] (lock-free reads, per-bucket
//! writes) and orders them in lock-free [`SegQueue`]s. Ghost is a fixed table
//! of atomic fingerprints: one store per eviction, one compare-exchange per
//! readmission, no allocation. Colliding fingerprints overwrite each other,
//! which only makes ghost readmission approximate.
//!
//! Entries removed from the index stay in their queue as stale entries and are
//! skipped when popped. Queue occupancy, stale entries included, is bounded by
//! [`QUEUE_BOUND_FACTOR`] times the shard capacity; a push over the bound first
//! retires the queue head.

use std::{
    hash::Hash,
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU64, Ordering},
    },
};

use ahash::RandomState;
use crossbeam_queue::SegQueue;
use scc::HashIndex;
use smallvec::SmallVec;

/// Saturation point of the per-entry hit counter.
const MAX_FREQ: u8 = 3;
/// Share of capacity, in percent, reserved for the probationary small queue.
const SMALL_PERCENT: usize = 10;
/// Queue occupancy bound, including stale entries, as a multiple of capacity.
const QUEUE_BOUND_FACTOR: usize = 2;
/// Maximum queue entries one eviction examines before evicting the current
/// candidate regardless of its hit counter. Without concurrent hits, an
/// eviction ends within `MAX_FREQ` passes over main; this bounds the work when
/// concurrent hits keep re-warming entries.
const EVICTION_PROBES: usize = 512;
/// Smallest shard capacity; smaller caches use fewer shards so per-shard
/// queues stay large enough for the small/main split to be meaningful.
const MIN_SHARD_CAPACITY: usize = 256;
/// Shards per available CPU, to keep contention on any one queue low.
const SHARDS_PER_CPU: usize = 4;

/// Keys displaced by one admission; usually zero or one.
pub(crate) type Evicted<K> = SmallVec<[K; 1]>;

/// Outcome of [`S3Fifo::admit`].
pub(crate) struct Admission<K> {
    /// Whether `key` became resident; `false` means it was already resident
    /// and the admission counted as a hit.
    pub(crate) inserted: bool,
    /// Keys evicted to keep the cache within capacity.
    pub(crate) evicted: Evicted<K>,
}

/// Bounded lock-free S3-FIFO residency set, sharded by key hash.
pub(crate) struct S3Fifo<K: 'static> {
    hasher: RandomState,
    shards: Box<[Shard<K>]>,
}

impl<K> S3Fifo<K>
where
    K: Copy + Eq + Hash + 'static,
{
    /// Creates an empty cache holding at most `capacity` keys (minimum one).
    pub(crate) fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let cpus = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        let count = cpus.saturating_mul(SHARDS_PER_CPU).min(capacity / MIN_SHARD_CAPACITY).max(1);
        let (base, extra) = (capacity / count, capacity % count);
        let shards =
            (0..count).map(|shard| Shard::new(base + usize::from(shard < extra))).collect();
        Self {
            hasher: RandomState::new(),
            shards,
        }
    }

    /// Records a hit on `key` if it is resident, without changing queue order.
    /// Lock-free. Returns whether `key` is resident.
    pub(crate) fn touch(&self, key: &K) -> bool {
        self.shard(self.hash(key)).touch(key)
    }

    /// Returns whether `key` is resident. Lock-free.
    pub(crate) fn contains(&self, key: &K) -> bool {
        self.shard(self.hash(key)).index.contains(key)
    }

    /// Records a hit on a resident `key`, or admits it and evicts keys as
    /// needed to stay within capacity.
    pub(crate) fn admit(&self, key: K) -> Admission<K> {
        let hash = self.hash(&key);
        self.shard(hash).admit(key, hash, &self.hasher)
    }

    /// Removes `key`, returning whether it was resident. Its queue entry is
    /// left behind as stale and skipped when popped.
    pub(crate) fn remove(&self, key: &K) -> bool {
        self.shard(self.hash(key)).remove(key)
    }

    fn hash(&self, key: &K) -> u64 {
        self.hasher.hash_one(key)
    }

    fn shard(&self, hash: u64) -> &Shard<K> {
        // Ghost slots use the low bits; pick shards from the high bits.
        let high = usize::try_from(hash >> 32).unwrap_or_default();
        &self.shards[high % self.shards.len()]
    }

    /// Returns the number of resident keys.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shards.iter().map(Shard::len).sum()
    }

    /// Number of queued entries in small and main, stale ones included.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> (usize, usize) {
        self.shards.iter().fold((0, 0), |(small, main), shard| {
            (small + shard.small.len(), main + shard.main.len())
        })
    }

    /// Sum of shard queue bounds, shared by small and main.
    #[cfg(test)]
    pub(crate) fn queue_bound(&self) -> usize {
        self.shards.iter().map(|shard| shard.queue_bound).sum()
    }

    /// Number of keys in the indexes; equals [`Self::len`] when quiescent.
    #[cfg(test)]
    pub(crate) fn indexed(&self) -> usize {
        self.shards.iter().map(|shard| shard.index.len()).sum()
    }

    /// Number of shards.
    #[cfg(test)]
    pub(crate) fn shard_count(&self) -> usize {
        self.shards.len()
    }
}

/// One resident key shared between the index and its current queue.
struct Node<K> {
    key: K,
    freq: AtomicU8,
    /// Set after the node leaves the index; queues skip it when popped.
    removed: AtomicBool,
}

impl<K> Node<K> {
    fn new(key: K) -> Self {
        Self {
            key,
            freq: AtomicU8::new(0),
            removed: AtomicBool::new(false),
        }
    }

    /// Records one hit. A lost race under contention drops the increment,
    /// which only makes the frequency estimate more conservative.
    fn hit(&self) {
        let freq = self.freq.load(Ordering::Relaxed);
        if freq < MAX_FREQ {
            let _ =
                self.freq.compare_exchange(freq, freq + 1, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    fn is_warm(&self) -> bool {
        self.freq.load(Ordering::Relaxed) > 0
    }

    /// Decrements the hit counter, returning whether it was non-zero.
    fn cool(&self) -> bool {
        self.freq
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |freq| {
                freq.checked_sub(1)
            })
            .is_ok()
    }

    fn is_removed(&self) -> bool {
        self.removed.load(Ordering::Acquire)
    }
}

/// Queue a popped node came from.
#[derive(Clone, Copy)]
enum Source {
    Small,
    Main,
}

/// Independent S3-FIFO instance over a hash partition of the keys.
struct Shard<K: 'static> {
    index: HashIndex<K, Arc<Node<K>>, RandomState>,
    small: SegQueue<Arc<Node<K>>>,
    main: SegQueue<Arc<Node<K>>>,
    /// Fingerprints of keys recently evicted from small; zero marks empty.
    ghost: Box<[AtomicU64]>,
    /// Resident entries. Signed because a concurrent removal can be counted
    /// before the admission it removes.
    len: AtomicIsize,
    capacity: isize,
    small_target: usize,
    queue_bound: usize,
}

impl<K> Shard<K>
where
    K: Copy + Eq + Hash + 'static,
{
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let small_target = (capacity * SMALL_PERCENT / 100).max(1);
        let ghost_slots = capacity.saturating_sub(small_target).max(1).next_power_of_two();
        Self {
            index: HashIndex::with_hasher(RandomState::new()),
            small: SegQueue::new(),
            main: SegQueue::new(),
            ghost: (0..ghost_slots).map(|_| AtomicU64::new(0)).collect(),
            len: AtomicIsize::new(0),
            capacity: isize::try_from(capacity).unwrap_or(isize::MAX),
            small_target,
            queue_bound: capacity.saturating_mul(QUEUE_BOUND_FACTOR),
        }
    }

    fn touch(&self, key: &K) -> bool {
        self.index.peek_with(key, |_, node| node.hit()).is_some()
    }

    fn admit(&self, key: K, hash: u64, hasher: &RandomState) -> Admission<K> {
        let mut evicted = Evicted::new();
        if self.touch(&key) {
            return Admission { inserted: false, evicted };
        }
        let node = Arc::new(Node::new(key));
        if self.index.insert_sync(key, node.clone()).is_err() {
            // A concurrent admission of the same key won; count it as a hit.
            self.touch(&key);
            return Admission { inserted: false, evicted };
        }
        let len = self.len.fetch_add(1, Ordering::AcqRel) + 1;
        if self.take_ghost(hash) {
            self.push_main(node, &mut evicted);
        } else {
            self.push_small(node, hasher, &mut evicted);
        }
        if evicted.is_empty() && len > self.capacity {
            self.evict_one(hasher, &mut evicted);
        }
        Admission { inserted: true, evicted }
    }

    fn remove(&self, key: &K) -> bool {
        let mut removed = None;
        self.index.remove_if_sync(key, |node| {
            removed = Some(node.clone());
            true
        });
        let Some(node) = removed else {
            return false;
        };
        node.removed.store(true, Ordering::Release);
        self.len.fetch_sub(1, Ordering::AcqRel);
        true
    }

    /// Evicts one resident key, examining at most [`EVICTION_PROBES`] entries.
    fn evict_one(&self, hasher: &RandomState, evicted: &mut Evicted<K>) {
        for probe in 1..=EVICTION_PROBES {
            let Some((node, source)) = self.pop_candidate() else {
                return;
            };
            if node.is_removed() {
                continue;
            }
            let force = probe == EVICTION_PROBES;
            match source {
                Source::Small if !force && node.is_warm() => self.push_main(node, evicted),
                Source::Small => {
                    self.evict_node(&node, Some(hasher), evicted);
                }
                Source::Main if !force && node.cool() => self.push_main(node, evicted),
                Source::Main => {
                    self.evict_node(&node, None, evicted);
                }
            }
            if !evicted.is_empty() {
                return;
            }
        }
    }

    /// Pops from small while it holds at least its target share, otherwise
    /// from main, falling back to the other queue when one is empty.
    fn pop_candidate(&self) -> Option<(Arc<Node<K>>, Source)> {
        let small = || self.small.pop().map(|node| (node, Source::Small));
        let main = || self.main.pop().map(|node| (node, Source::Main));
        if self.small.len() >= self.small_target {
            small().or_else(main)
        } else {
            main().or_else(small)
        }
    }

    /// Appends `node` to small, retiring heads while the queue is at its bound.
    fn push_small(&self, node: Arc<Node<K>>, hasher: &RandomState, evicted: &mut Evicted<K>) {
        while self.small.len() >= self.queue_bound {
            let Some(head) = self.small.pop() else {
                break;
            };
            if head.is_removed() {
                continue;
            }
            if head.is_warm() {
                self.push_main(head, evicted);
            } else {
                self.evict_node(&head, Some(hasher), evicted);
            }
        }
        self.small.push(node);
    }

    /// Appends `node` to main, evicting heads while the queue is at its bound.
    ///
    /// Main only reaches its bound when stale entries or concurrent overshoot
    /// fill it, so evicting a live head here regardless of frequency is rare.
    fn push_main(&self, node: Arc<Node<K>>, evicted: &mut Evicted<K>) {
        while self.main.len() >= self.queue_bound {
            let Some(head) = self.main.pop() else {
                break;
            };
            if !head.is_removed() {
                self.evict_node(&head, None, evicted);
            }
        }
        self.main.push(node);
    }

    /// Removes `node` from the index if it is still the resident entry for
    /// its key and records the key in `evicted`. With `ghost`, also remembers
    /// the key for readmission into main.
    fn evict_node(
        &self,
        node: &Arc<Node<K>>,
        ghost: Option<&RandomState>,
        evicted: &mut Evicted<K>,
    ) -> bool {
        let key = node.key;
        if !self.index.remove_if_sync(&key, |resident| Arc::ptr_eq(resident, node)) {
            return false;
        }
        node.removed.store(true, Ordering::Release);
        self.len.fetch_sub(1, Ordering::AcqRel);
        if let Some(hasher) = ghost {
            let hash = hasher.hash_one(key);
            self.ghost_slot(hash).store(fingerprint(hash), Ordering::Relaxed);
        }
        evicted.push(key);
        true
    }

    /// Consumes the ghost fingerprint for `hash`, returning whether it was set.
    fn take_ghost(&self, hash: u64) -> bool {
        let slot = self.ghost_slot(hash);
        let fingerprint = fingerprint(hash);
        slot.load(Ordering::Relaxed) == fingerprint
            && slot
                .compare_exchange(fingerprint, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }

    fn ghost_slot(&self, hash: u64) -> &AtomicU64 {
        let low = usize::try_from(hash & u64::from(u32::MAX)).unwrap_or_default();
        &self.ghost[low & (self.ghost.len() - 1)]
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.len.load(Ordering::Acquire).max(0).unsigned_abs()
    }
}

/// Non-zero ghost fingerprint of a key hash.
fn fingerprint(hash: u64) -> u64 {
    hash | 1
}
