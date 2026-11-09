//! S3-FIFO residency policy: capacity bound, frequency-aware retention, ghost
//! readmission, stale-entry bounds, and concurrent accounting.

use std::{collections::HashSet, sync::Arc};

use solana_account::AccountMode;
use solana_pubkey::Pubkey;

use crate::{cache::AccountCache, s3fifo::S3Fifo};

/// Admits `keys` in order and returns every key reported as evicted.
fn admit_all(cache: &S3Fifo<u64>, keys: impl IntoIterator<Item = u64>) -> Vec<u64> {
    keys.into_iter().flat_map(|key| cache.admit(key).evicted).collect()
}

/// Proves residency never exceeds capacity and every displaced key is
/// reported exactly once and is no longer resident.
#[test]
fn s3fifo_bounds_residency_and_reports_every_eviction() {
    let cache = S3Fifo::new(16);
    let evicted = admit_all(&cache, 0..64);

    assert_eq!(cache.len(), 16);
    assert_eq!(evicted.len(), 48);
    let evicted: HashSet<_> = evicted.into_iter().collect();
    assert_eq!(evicted.len(), 48, "each key is reported once");
    for key in 0..64 {
        assert_ne!(
            cache.contains(&key),
            evicted.contains(&key),
            "key {key} is either resident or reported evicted"
        );
    }
}

/// Proves keys hit while resident survive a scan of one-off keys many times
/// larger than the cache, which an LRU would flush entirely.
#[test]
fn s3fifo_retains_hot_keys_through_a_scan() {
    let cache = S3Fifo::new(100);
    let hot = 0..50;
    admit_all(&cache, hot.clone());
    for key in hot.clone() {
        assert!(cache.touch(&key));
        assert!(
            !cache.admit(key).inserted,
            "admitting a resident key is a hit"
        );
    }

    let evicted = admit_all(&cache, 1_000..11_000);

    assert!(
        hot.clone().all(|key| cache.contains(&key)),
        "hot keys stay resident"
    );
    assert!(
        evicted.iter().all(|key| *key >= 1_000),
        "only scan keys are evicted"
    );
    assert_eq!(cache.len(), 100);
}

/// Proves a key evicted from probation and admitted again soon after skips
/// probation, so it outlives later one-off keys.
#[test]
fn s3fifo_readmits_recent_ghosts_into_main() {
    let cache = S3Fifo::new(10);
    admit_all(&cache, 0..=10);
    assert!(
        !cache.contains(&0),
        "the coldest probationary key is evicted"
    );

    assert!(cache.admit(0).inserted);
    // Without ghost readmission, key 0 would reach the probation head within
    // ten admissions and be evicted.
    admit_all(&cache, 100..120);

    assert!(cache.contains(&0), "a ghost readmission lands in main");
}

/// Proves touching an absent key neither admits it nor disturbs residency.
#[test]
fn s3fifo_touch_ignores_absent_keys() {
    let cache = S3Fifo::new(4);
    admit_all(&cache, 0..4);

    assert!(!cache.touch(&99));
    assert!(!cache.contains(&99));
    assert_eq!(cache.len(), 4);
}

/// Proves stale queue entries left by removals stay within the queue bound and
/// never count against residency.
#[test]
fn s3fifo_removal_churn_keeps_queues_bounded() {
    let cache = S3Fifo::new(8);
    for key in 0..1_000 {
        assert!(cache.admit(key % 3).inserted);
        assert!(cache.remove(&(key % 3)));
        assert!(!cache.remove(&(key % 3)), "a second removal finds nothing");
        let (small, main) = cache.queued();
        assert!(small + main <= 2 * cache.queue_bound());
    }
    assert_eq!(cache.len(), 0);

    let evicted = admit_all(&cache, 100..108);
    assert!(
        evicted.is_empty(),
        "stale entries do not displace live keys"
    );
    assert_eq!(cache.len(), 8);
}

/// Proves an admission over capacity still evicts a live key when more stale
/// entries than the probe budget sit ahead of it in the queue.
#[test]
fn s3fifo_stale_backlog_does_not_block_eviction() {
    const CAPACITY: usize = 1_024;
    let cache = S3Fifo::with_shard_count(CAPACITY, 1);
    let capacity = u64::try_from(CAPACITY).unwrap();
    for key in 0..capacity {
        assert!(cache.admit(key).inserted);
        assert!(cache.remove(&key));
    }
    admit_all(&cache, capacity..2 * capacity);
    assert_eq!(cache.len(), CAPACITY);

    let admission = cache.admit(2 * capacity);

    assert_eq!(admission.evicted.len(), 1, "one live key is displaced");
    assert_eq!(cache.len(), CAPACITY, "residency stays within capacity");
}

/// Proves concurrent admissions, hits, and removals keep the resident count,
/// the indexes, and the reported evictions consistent and within bounds, for
/// both a single shard and a sharded cache.
#[test]
fn s3fifo_concurrent_accounting_is_consistent() {
    const THREADS: u64 = 8;
    for capacity in [64, 4_096] {
        let cache = S3Fifo::new(capacity);
        if capacity > 256 {
            assert!(cache.shard_count() > 1, "large caches are sharded");
        }
        let keys = u64::try_from(capacity * 8).unwrap();

        let (inserted, evicted, removed) = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..THREADS)
                .map(|thread| {
                    let cache = &cache;
                    scope.spawn(move || churn(cache, thread, keys))
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .fold((0, 0, 0), |(i, e, r), (wi, we, wr)| {
                    (i + wi, e + we, r + wr)
                })
        });

        assert_eq!(cache.len(), cache.indexed(), "counters match the indexes");
        assert_eq!(
            inserted - evicted - removed,
            cache.len(),
            "every exit is reported"
        );
        assert!(cache.len() <= capacity);
        let (small, main) = cache.queued();
        let slack = usize::try_from(THREADS).unwrap() * cache.shard_count();
        assert!(small <= cache.queue_bound() + slack);
        assert!(main <= cache.queue_bound() + slack);
    }
}

/// Runs a random mix of admissions, hits, and removals over `keys` keys and
/// returns the observed (inserted, evicted, removed) counts.
fn churn(cache: &S3Fifo<u64>, thread: u64, keys: u64) -> (usize, usize, usize) {
    const OPS: u64 = 20_000;
    let (mut inserted, mut evicted, mut removed) = (0usize, 0usize, 0usize);
    let mut state = 0x9E37_79B9_7F4A_7C15 ^ (thread + 1);
    for _ in 0..OPS {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let key = state % keys;
        match state % 20 {
            0 => removed += usize::from(cache.remove(&key)),
            1..=5 => {
                cache.touch(&key);
            }
            _ => {
                let admission = cache.admit(key);
                inserted += usize::from(admission.inserted);
                evicted += admission.evicted.len();
            }
        }
    }
    (inserted, evicted, removed)
}

/// Proves materializing past capacity publishes the displaced account to the
/// eviction subscriber, and a later eviction check for it still applies.
#[tokio::test]
async fn account_cache_publishes_displaced_accounts() {
    let cache = Arc::new(AccountCache::new(2));
    let mut evictions = cache.evictions.subscribe().unwrap();
    let keys: Vec<_> = (0..3).map(|_| Pubkey::new_unique()).collect();
    for key in &keys {
        let mut lease = cache.lock(*key).await;
        lease.observed = Some((AccountMode::Mirrored, 0));
        lease.materialized(AccountMode::Mirrored);
    }

    let evicted = evictions.try_recv().expect("one account is displaced");
    assert_eq!(evicted, keys[0]);
    assert!(
        evictions.try_recv().is_err(),
        "only one account is displaced"
    );
    let mut lease = cache.lock(evicted).await;
    lease.observed = Some((AccountMode::Mirrored, 0));
    assert!(lease.cached_eviction_applies());
}
