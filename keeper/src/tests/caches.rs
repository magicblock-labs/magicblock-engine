//! Read-side cache primitives keeper owns: the slot-based `ExpiringCache` and the
//! `AccountCache` account-mutation coordination.

use std::sync::Arc;

use solana_account::AccountMode;
use solana_pubkey::Pubkey;

use crate::cache::{AccountCache, ExpiringCache};

// `ExpiringCache` evicts lazily on push, never on read; re-inserting an existing
// key is a no-op; `update` replaces only present values.
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

    // Eviction runs only on push: at slot 5 the entry is well past its expiry but
    // stays readable until the next push sweeps the queue.
    assert!(cache.contains(&1));
    assert_eq!(cache.get(&1), Some(10));

    // A push at slot 5 first evicts everything expired at 5 (key 1), then inserts.
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
}

/// Proves an accessor holds mutation ownership across materialization, while
/// mode changes, deletion, and stale eviction checks keep recency coherent.
#[tokio::test]
async fn account_lease_coordinates_recency_and_waiters() {
    use AccountMode::*;
    let modes = [ReadOnly, Placeholder, Delegated, Ephemeral, Transient, System];
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

        let tracked = matches!(mode, ReadOnly | Placeholder | System);
        assert_eq!(cache.lru.get_sync(&pk).is_some(), tracked);
    }

    let cache = Arc::new(AccountCache::new(256));
    let pk = Pubkey::new_unique();
    let lease = cache.lock(pk).await;
    lease.materialized(ReadOnly).await;
    drop(lease);
    assert!(cache.lru.get_sync(&pk).is_some(), "read-only is admitted");
    let eviction = cache.lock(pk).await;
    assert!(
        !eviction.cached_eviction_applies(ReadOnly),
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

    let lease = cache.lock(pk).await;
    assert!(
        lease.cached_eviction_applies(ReadOnly),
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
