//! Subscription fanout primitives, transaction-append dedup

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
