//! Recency eviction under RPC fan-out: accounts read on every request must
//! stay resident while each request also touches many one-off accounts.
//!
//! `missing_accounts` returns exactly the accounts a synchronizing caller
//! fetches upstream, so its size per round is the upstream fetch cost. The
//! test drains evictions the way the validator does: claim the cached eviction
//! under the account lease and delete the account.
#![cfg(test)]

use engine::{
    Engine,
    testkit::{Pacing, TestEngine},
};
use keeper::testkit::{Dirs, keeper_builder};
use solana_account::{AccountBuilder, AccountMode};
use solana_pubkey::Pubkey;
use tokio::sync::mpsc::UnboundedReceiver;

const CAPACITY: usize = 256;
const HOT: usize = 50;
const ONE_OFF_PER_ROUND: usize = 300;
const BATCH: usize = 100;
const ROUNDS: usize = 15;
/// Rounds allowed for hot accounts to graduate from probation. Each round's
/// one-off traffic exceeds the cache, so a hot account evicted before its
/// first hit reaches main only if its ghost entry survives the round (about
/// two thirds of the time here). Stragglers shrink geometrically, so after
/// this many rounds the chance any of the 50 remains is about 0.1%.
const WARMUP_ROUNDS: usize = 12;

/// Materializes every requested account the engine reports missing, then
/// applies the resulting evictions. Returns how many accounts were fetched.
async fn request(
    engine: &Engine,
    evictions: &mut UnboundedReceiver<Pubkey>,
    keys: &[Pubkey],
    owner: Pubkey,
) -> usize {
    let missing = engine.missing_accounts(keys).await.unwrap();
    let fetched = missing.len();
    for accessor in missing {
        let account = AccountBuilder::default()
            .lamports(2_000_000)
            .owner(owner)
            .mode(AccountMode::Mirrored)
            .slot(1)
            .data(vec![1]);
        accessor.materialize(account, None).await.unwrap();
    }
    while let Ok(evicted) = evictions.try_recv() {
        let accessor = engine.account(evicted).await.unwrap();
        if let Some(accessor) = accessor.into_cached_eviction() {
            accessor.delete().await.unwrap();
        }
    }
    fetched
}

/// Proves hot accounts stop being re-fetched once they graduate from
/// probation, while every request also fans out to one-off accounts that
/// exceed the whole cache. An LRU re-fetches most of the hot set every round.
#[tokio::test(flavor = "multi_thread")]
async fn hot_accounts_survive_one_off_fanout() {
    let dirs = Dirs::default();
    let mut builder = keeper_builder(&dirs);
    builder.accountsdb.lru_capacity = CAPACITY;
    let te = TestEngine::from_builder(dirs, builder, Pacing::External).await;
    let mut evictions = te.accounts().subscribe_evictions().unwrap();
    let owner = Pubkey::new_unique();
    let hot: Vec<_> = (0..HOT).map(|_| Pubkey::new_unique()).collect();

    let mut refetched_per_round = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        refetched_per_round.push(request(&te, &mut evictions, &hot, owner).await);
        let one_off: Vec<_> = (0..ONE_OFF_PER_ROUND).map(|_| Pubkey::new_unique()).collect();
        for batch in one_off.chunks(BATCH) {
            assert_eq!(
                request(&te, &mut evictions, batch, owner).await,
                batch.len(),
                "one-off accounts are always fetched"
            );
        }
    }

    println!("hot accounts fetched per round: {refetched_per_round:?}");
    assert!(
        refetched_per_round.iter().skip(WARMUP_ROUNDS).all(|count| *count == 0),
        "hot accounts are still evicted by one-off traffic and re-fetched upstream \
         after {WARMUP_ROUNDS} warm-up rounds: {refetched_per_round:?}"
    );
}
