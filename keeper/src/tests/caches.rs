//! Read-side cache primitives keeper owns: the slot-based `ExpiringCache` and the
//! `AccountCache` missing-load coordination.

use std::sync::Arc;

use solana_account::{AccountBuilder, AccountMode};
use solana_pubkey::Pubkey;

use super::TestKeeper;
use crate::cache::{AccountCache, AccountLoad, AccountWait, ExpiringCache, MissingAccount};

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

// Two callers racing on the same missing account get exactly one loader and one
// waiter; committing caches the account while dropping the load guard does not.
#[tokio::test]
async fn account_load_release_paths_wake_waiters() {
    use AccountMode::*;
    let modes = [ReadOnly, Placeholder, Delegated, Ephemeral, Transient, System];
    for mode in modes {
        for commit in [true, false] {
            let cache = Arc::new(AccountCache::new(256));
            let pk = Pubkey::new_unique();
            let (load, wait) = reserve_load_and_wait(&cache, pk);

            let waiter = tokio::spawn(async move { wait.wait().await });
            if commit {
                load.complete(mode);
            } else {
                drop(load)
            }

            assert_eq!(
                waiter.await.unwrap(),
                (pk, commit),
                "waiter returns the load outcome"
            );
            let tracked = matches!(mode, ReadOnly | Placeholder | System);
            assert_eq!(cache.lru.get_sync(&pk).is_some(), tracked && commit);
            assert!(matches!(cache.reserve(pk), MissingAccount::Load(_)));
        }
    }
}

// `ensure` is the production seam over `AccountCache`: it skips accounts already
// resident in storage (promoting them) and hands back a coordination item only
// for the ones missing, with the first caller owning the load.
#[tokio::test]
async fn ensure_reserves_only_missing_accounts() {
    let keeper = TestKeeper::new().await;
    let present = Pubkey::new_unique();
    let missing = Pubkey::new_unique();
    keeper
        .accounts()
        .store(&[(present, AccountBuilder::default().lamports(1).build())])
        .unwrap();

    // The accessor must outlive the iterator that borrows it.
    let accounts = keeper.accounts();
    let reserved: Vec<_> = accounts.ensure(&[present, missing]).collect();

    // The resident account is skipped entirely; only the missing one surfaces,
    // and the first caller to reach it owns the load.
    assert_eq!(reserved.len(), 1, "only the missing account is reserved");
    let MissingAccount::Load(load) = &reserved[0] else {
        panic!("first reservation of a missing account owns the load");
    };
    assert_eq!(load.pubkey, missing);

    // The reservation stays live while the load guard is held, so a concurrent
    // `ensure` of the same account waits instead of racing a second load.
    let again: Vec<_> = accounts.ensure(&[missing]).collect();
    assert!(matches!(again.as_slice(), [MissingAccount::Wait(_)]));

    keeper.close().await;
}

fn reserve_load_and_wait(cache: &Arc<AccountCache>, pk: Pubkey) -> (AccountLoad, AccountWait) {
    let MissingAccount::Load(load) = cache.reserve(pk) else {
        panic!("first reservation must own the load");
    };
    let MissingAccount::Wait(wait) = cache.reserve(pk) else {
        panic!("concurrent reservation must wait");
    };
    (load, wait)
}
