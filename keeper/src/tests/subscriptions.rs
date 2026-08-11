//! Subscription fanout primitives, transaction-append dedup

use std::sync::Arc;

use super::{TestKeeper, signed_tx};
use crate::{
    ResolvedTransaction,
    subscriptions::{Multicast, MulticastOneshot, Subscription, Unicast},
};
use nucleus::testkit::{V42_ID, signed_view};
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_transaction_error::TransactionError;

/// Proves unicast exclusivity, persistent fanout, terminal fanout, and slow-receiver removal.
#[tokio::test]
async fn subscribers_send_semantics() {
    let unicast = Arc::new(Unicast::new(1, Subscription::Transactions));
    let mut unicast_rx = unicast.subscribe().unwrap();
    assert!(unicast.subscribe().is_err());
    unicast.send(1).await;
    let sender = unicast.clone();
    let send = tokio::spawn(async move { sender.send(2).await });
    tokio::task::yield_now().await;
    assert!(!send.is_finished(), "async unicast send waits for capacity");
    assert_eq!(unicast_rx.recv().await, Some(1));
    send.await.unwrap();
    assert_eq!(unicast_rx.recv().await, Some(2));

    unicast.send(3).await;
    let sender = unicast.clone();
    let send = std::thread::spawn(move || sender.blocking_send(4));
    assert_eq!(unicast_rx.recv().await, Some(3));
    send.join().unwrap();
    assert_eq!(unicast_rx.recv().await, Some(4));
    drop(unicast_rx);
    assert!(unicast.subscribe().is_err());

    let multicast = Multicast::new(1, Subscription::Accounts);
    multicast.send(&1, &9);
    let mut first = multicast.subscribe(1).await;
    let mut second = multicast.subscribe(1).await;
    multicast.send(&1, &10);
    assert_eq!(first.recv().await, Some(10));
    assert_eq!(second.recv().await, Some(10));
    multicast.send(&1, &11);
    multicast.send(&1, &12);
    assert_eq!(first.recv().await, Some(11));
    assert_eq!(first.recv().await, None);
    assert_eq!(second.recv().await, Some(11));
    assert_eq!(second.recv().await, None);

    let oneshot = MulticastOneshot::default();
    let first = oneshot.subscribe(1).await;
    let closed = oneshot.subscribe(1).await;
    drop(closed);
    oneshot.send_last(&1, &18);
    assert!(matches!(
        first.try_recv(),
        Err(oneshot::TryRecvError::Empty)
    ));
    let second = oneshot.subscribe(1).await;
    oneshot.send_last(&1, &19);
    assert_eq!(second.await.unwrap(), 19);
    oneshot.send(&1, &20);
    assert_eq!(first.await.unwrap(), 20);
    let third = oneshot.subscribe(1).await;
    oneshot.send(&1, &21);
    assert_eq!(third.await.unwrap(), 21);
}

// Appending reserves the signature while rejection wakes only its own latest
// waiter. Invalid blockhash is retained as a terminal cached status.
#[tokio::test]
async fn append_dedup_and_status_sentinel() {
    let keeper = TestKeeper::new().await;
    let (signature, txn) = signed_tx();
    let slot = keeper.blocks().current_slot();
    let original = keeper.transactions().subscribe_signature(signature).await;

    // First append writes to the ledger; the duplicate is dropped.
    assert!(
        keeper.transactions().append(&txn).await.unwrap(),
        "first append is accepted"
    );
    let duplicate = keeper.transactions().subscribe_signature(signature).await;
    assert!(
        !keeper.transactions().append(&txn).await.unwrap(),
        "duplicate is deduplicated"
    );
    let status = duplicate.await.unwrap();
    assert_eq!(status.result, Err(TransactionError::AlreadyProcessed));
    assert_eq!(status.slot, slot);
    assert!(matches!(
        original.try_recv(),
        Err(oneshot::TryRecvError::Empty)
    ));

    // The sentinel makes status() return None from the cache.
    assert!(keeper.transactions().status(signature).await.unwrap().is_none());

    let payer = Keypair::new();
    let (signature, view) = signed_view(
        &payer,
        [Instruction::new_with_bytes(V42_ID, &[], vec![])],
        Hash::new_from_array([1; 32]),
    );
    let txn =
        ResolvedTransaction::try_new(view, Some(Default::default()), &Default::default()).unwrap();
    let rejected = keeper.transactions().subscribe_signature(signature).await;
    assert!(!keeper.transactions().append(&txn).await.unwrap());
    let status = rejected.await.unwrap();
    assert_eq!(status.result, Err(TransactionError::BlockhashNotFound));
    assert_eq!(status.slot, slot);
    let cached = keeper.transactions().status(signature).await.unwrap().unwrap();
    assert_eq!(cached.result, Err(TransactionError::BlockhashNotFound));
    assert_eq!(cached.slot, slot);

    keeper.close().await;
}
