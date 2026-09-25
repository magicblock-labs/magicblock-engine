//! Read-side cache primitives keeper owns: the slot-based `ExpiringCache` and the
//! `AccountCache` account-mutation coordination.

use std::sync::Arc;

use solana_account::AccountMode;
use solana_pubkey::Pubkey;

use crate::cache::{AccountCache, EVICTION_LIMIT, ExpiringCache};

/// Proves lazy eviction respects the per-push eviction limit, successive pushes
/// drain expired entries, and duplicates are rejected until swept; updates
/// affect only present keys.
#[test]
fn expiring_cache_lazy_eviction() {
    // ttl = 2 slots: a key pushed at slot s expires at s + 2.
    let cache: ExpiringCache<u64, u64> = ExpiringCache::new(2);

    assert!(cache.push(1, 10, 0)); // inserted, expires at slot 2
    assert!(!cache.push(1, 99, 0)); // re-insert of an existing key is a no-op
    assert_eq!(
        cache.get(&1),
        Some(10),
        "value left unchanged by the re-insert"
    );

    // Reads leave entries present until a push sweeps them.
    assert!(cache.contains(&1));
    assert_eq!(cache.get(&1), Some(10));

    assert!(cache.push(2, 20, 5));
    assert!(!cache.contains(&1), "expired key swept on the next push");
    assert_eq!(cache.get(&2), Some(20));

    // `update` replaces a present value and no-ops for an absent key.
    cache.update(&2, 21);
    assert_eq!(cache.get(&2), Some(21));
    cache.update(&404, 0);
    assert!(!cache.contains(&404));

    // A key re-admitted after expiry is a fresh insert again.
    assert!(cache.push(1, 11, 5));
    assert_eq!(cache.get(&1), Some(11));

    // Cross two full batches and a partial batch at the exact expiry slot.
    let limit = EVICTION_LIMIT as u64;
    let burst = 2 * limit + 2;
    let fresh = burst + 1;
    for key in 3..=burst {
        assert!(cache.push(key, key, 5));
    }
    assert!(!cache.push(burst, 99, 6));
    assert_eq!(
        cache.len(),
        burst as usize,
        "entries survive until their expiry slot"
    );

    assert!(
        !cache.push(burst, 99, 7),
        "unswept expired key rejects duplicates"
    );
    assert_eq!(
        cache.len(),
        (burst - limit) as usize,
        "even a rejected push sweeps a full batch"
    );
    assert!(!cache.contains(&limit));
    assert_eq!(
        cache.get(&(limit + 1)),
        Some(limit + 1),
        "reads do not sweep expired entries"
    );
    assert_eq!(
        cache.get(&burst),
        Some(burst),
        "duplicate leaves the value unchanged"
    );

    assert!(cache.push(fresh, fresh, 7));
    assert_eq!(
        cache.len(),
        3,
        "next push sweeps another full batch before inserting"
    );
    assert!(!cache.contains(&(2 * limit)));
    assert_eq!(cache.get(&(2 * limit + 1)), Some(2 * limit + 1));

    assert!(
        cache.push(burst, 99, 7),
        "swept key can be reinserted in the same push"
    );
    assert_eq!(
        cache.len(),
        2,
        "final push drains the remaining expired entries"
    );
    assert!(!cache.contains(&(2 * limit + 1)));
    assert_eq!(cache.get(&burst), Some(99));
    assert_eq!(
        cache.get(&fresh),
        Some(fresh),
        "unexpired entries are retained"
    );
}

/// Proves an accessor holds mutation ownership across materialization, while
/// mode changes, deletion, and stale eviction checks keep recency coherent.
#[tokio::test]
async fn account_lease_coordinates_recency_and_waiters() {
    use AccountMode::*;
    let modes = [ReadOnly, Uninit, Delegated, Magic, Transient, System];
    for mode in modes {
        let cache = Arc::new(AccountCache::new(256));
        let pk = Pubkey::new_unique();
        let lease = cache.lock(pk).await;
        let mut waiter = Box::pin(cache.lock(pk));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut waiter)
                .await
                .is_err()
        );

        lease.materialized(mode).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut waiter)
                .await
                .is_err(),
            "materialization retains ownership"
        );
        drop(lease);
        drop(waiter.await);

        let tracked = matches!(mode, ReadOnly | Uninit | System);
        assert_eq!(cache.lru.get_sync(&pk).is_some(), tracked);
    }

    let cache = Arc::new(AccountCache::new(256));
    let pk = Pubkey::new_unique();
    let lease = cache.lock(pk).await;
    lease.materialized(ReadOnly).await;
    drop(lease);
    assert!(cache.lru.get_sync(&pk).is_some(), "read-only is admitted");
    let mut eviction = cache.lock(pk).await;
    eviction.observed = Some((ReadOnly, 0));
    assert!(
        !eviction.cached_eviction_applies(),
        "an older eviction is rejected after readmission"
    );
    drop(eviction);
    assert!(
        cache.lru.get_sync(&pk).is_some(),
        "rejecting stale eviction leaves recency unchanged"
    );

    let lease = cache.lock(pk).await;
    lease.materialized(Delegated).await;
    drop(lease);
    assert!(
        cache.lru.get_sync(&pk).is_none(),
        "authoritative transition removes recency"
    );

    let mut lease = cache.lock(pk).await;
    lease.observed = Some((ReadOnly, 0));
    assert!(
        lease.cached_eviction_applies(),
        "an account absent from recency remains eligible for eviction"
    );
    lease.materialized(ReadOnly).await;
    lease.deleted();
    drop(lease);
    assert!(
        cache.lru.get_sync(&pk).is_none(),
        "deletion removes recency"
    );
}
