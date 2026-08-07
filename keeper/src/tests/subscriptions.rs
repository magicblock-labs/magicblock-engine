//! Subscription fanout primitives, transaction-append dedup

use ledger::request::TransactionStatus;

use super::{TestKeeper, signed_tx};
use crate::subscriptions::Subscribers;

// A keyed broadcast map creates channels lazily, drops the channel after a
// oneshot send or once its last receiver is gone, and treats an absent key as a
// successful no-op.
#[tokio::test]
async fn subscribers_send_semantics() {
    let subs: Subscribers<u64, u64> = Subscribers::default();

    // Sending to a key with no channel is a no-op (no panic, nothing removed).
    subs.send(&1, &10, false);

    // A subscriber receives ordinary (non-oneshot) sends on its key.
    let mut rx = subs.subscribe(1, 4).await;
    subs.send(&1, &11, false);
    assert_eq!(rx.recv().await.unwrap(), 11);

    // A oneshot send delivers, then removes the channel; a fresh subscribe gets a
    // new channel that never sees the terminal value.
    subs.send(&1, &12, true);
    assert_eq!(rx.recv().await.unwrap(), 12);
    let mut rx2 = subs.subscribe(1, 4).await;
    subs.send(&1, &13, false);
    assert_eq!(rx2.recv().await.unwrap(), 13);

    // A send to a key whose only receiver has been dropped removes the dead
    // channel; the next subscribe recreates it without redelivering old values.
    let mut rx3 = subs.subscribe(2, 4).await;
    subs.send(&2, &20, false);
    assert_eq!(rx3.recv().await.unwrap(), 20);
    drop(rx3);
    subs.send(&2, &21, false); // dead channel -> removed
    let mut rx4 = subs.subscribe(2, 4).await;
    subs.send(&2, &22, false);
    assert_eq!(rx4.recv().await.unwrap(), 22);
}

// Appending a transaction records a `Some(None)` sentinel in the signature cache,
// so a same-slot re-append is deduplicated and `status` is served from the cache
// as "seen, no status yet" without a ledger read.
#[tokio::test]
async fn append_dedup_and_status_sentinel() {
    let keeper = TestKeeper::new().await;
    let (signature, txn) = signed_tx();

    // First append writes to the ledger; the duplicate is dropped.
    assert!(
        keeper.transactions().append(&txn).await.unwrap(),
        "first append is accepted"
    );
    assert!(
        !keeper.transactions().append(&txn).await.unwrap(),
        "duplicate is deduplicated"
    );

    // The sentinel makes status() return None from the cache.
    assert!(keeper.transactions().status(signature).await.unwrap().is_none());

    keeper.close().await;
}

// A duplicate submitted after the original settled must still be told the
// original's outcome.
//
// Waiters are keyed by signature and the settling broadcast is terminal, so it
// takes the channel with it. A duplicate arriving afterwards subscribes to a
// freshly created channel that nothing will ever write to again, and without
// `notify_duplicate` it simply blocks until its caller's own deadline — which is
// how a transaction that executed perfectly well gets reported upstream as a
// timeout.
#[tokio::test]
async fn duplicate_after_settlement_is_served_the_original_status() {
    let keeper = TestKeeper::new().await;
    let (signature, txn) = signed_tx();
    assert!(keeper.transactions().append(&txn).await.unwrap());

    // Settle it the way `commit_execution` does: cache first, then the terminal
    // broadcast that drops the channel.
    let settled = TransactionStatus {
        result: Ok(()),
        slot: 7,
    };
    keeper.caches.signatures.update(&signature, Some(settled.clone()));
    keeper.subscriptions.signatures.send(&signature, &settled, true);

    // The late duplicate: subscribes to a channel created after the fact, so
    // only the cache can serve it.
    let mut rx = keeper.transactions().subscribe_signature(signature).await;
    assert!(
        !keeper.transactions().append(&txn).await.unwrap(),
        "duplicate is still deduplicated"
    );
    keeper.transactions().notify_duplicate(signature);

    let received = rx.try_recv().expect("late duplicate is served the settled status");
    assert_eq!(received.slot, settled.slot);
    assert!(received.result.is_ok());

    keeper.close().await;
}

// While the original is still in flight there is nothing to replay, and
// replaying a sentinel would settle the waiter on a status that does not exist
// yet. The shared channel is what serves both submitters in that case.
#[tokio::test]
async fn duplicate_while_in_flight_is_left_to_the_shared_channel() {
    let keeper = TestKeeper::new().await;
    let (signature, txn) = signed_tx();
    assert!(keeper.transactions().append(&txn).await.unwrap());

    let mut rx = keeper.transactions().subscribe_signature(signature).await;
    keeper.transactions().notify_duplicate(signature);
    assert!(rx.try_recv().is_err(), "nothing is published for an unsettled signature");

    // ...and when it does settle, that same channel delivers.
    let settled = TransactionStatus {
        result: Ok(()),
        slot: 9,
    };
    keeper.caches.signatures.update(&signature, Some(settled.clone()));
    keeper.subscriptions.signatures.send(&signature, &settled, true);
    assert_eq!(rx.try_recv().expect("in-flight waiter is served").slot, 9);

    keeper.close().await;
}
