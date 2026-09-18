//! Subscription fanout primitives, transaction-append dedup

use std::sync::Arc;

use super::{TestKeeper, signed_tx};
use crate::{
    ResolvedTransaction, TransactionStatus,
    subscriptions::{Multicast, Signatures, Subscription, Unicast},
};
use nucleus::testkit::{V42_ID, signed_view};
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signature::Signature;
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
    let send = std::thread::spawn(move || sender.blocking_send(|| 4));
    assert_eq!(unicast_rx.recv().await, Some(3));
    send.join().unwrap();
    assert_eq!(unicast_rx.recv().await, Some(4));
    drop(unicast_rx);
    assert!(unicast.subscribe().is_err());

    let multicast = Multicast::new(1, Subscription::Accounts);
    multicast.send(&1, &9);
    assert!(!multicast.contains(&1));
    let mut first = multicast.subscribe(1);
    let mut second = multicast.subscribe(1);
    multicast.send(&1, &10);
    assert_eq!(first.recv().await, Some(10));
    assert_eq!(second.recv().await, Some(10));
    multicast.send(&1, &11);
    multicast.send(&1, &12);
    assert_eq!(first.recv().await, Some(11));
    assert_eq!(first.recv().await, None);
    assert_eq!(second.recv().await, Some(11));
    assert_eq!(second.recv().await, None);

    // Overflow removed the last receivers. Registration must reopen
    // delivery, and pruning a different key must not hide the remaining one.
    let mut live = multicast.subscribe(2);
    let closed = multicast.subscribe(3);
    drop(closed);
    assert!(!multicast.contains(&3));
    multicast.send(&2, &13);
    assert_eq!(live.recv().await, Some(13));
    drop(live);
    assert!(!multicast.contains(&2));

    let signatures = Signatures::default();
    let signature = Signature::from([1; 64]);
    signatures.send(&signature, &TransactionStatus { slot: 19, result: Ok(()) });
    let first = signatures.subscribe(signature);
    let closed = signatures.subscribe(signature);
    drop(closed);
    signatures.cleanup().await;
    assert!(matches!(
        first.try_recv(),
        Err(oneshot::TryRecvError::Empty)
    ));
    let second = signatures.subscribe(signature);
    signatures.send(&signature, &TransactionStatus { slot: 20, result: Ok(()) });
    assert_eq!(first.await.unwrap().slot, 20);
    assert_eq!(second.await.unwrap().slot, 20);
    let third = signatures.subscribe(signature);
    signatures.send(&signature, &TransactionStatus { slot: 21, result: Ok(()) });
    assert_eq!(third.await.unwrap().slot, 21);
    let closed = signatures.subscribe(signature);
    drop(closed);
    signatures.cleanup().await;
    signatures.cleanup().await;
    let reopened = signatures.subscribe(signature);
    signatures.send(&signature, &TransactionStatus { slot: 22, result: Ok(()) });
    assert_eq!(reopened.await.unwrap().slot, 22);
}

/// Proves admission rejections reserve signatures without publishing execution
/// status or consuming any observer, including observers of invalid blockhashes.
#[tokio::test]
async fn append_dedup_and_status_sentinel() {
    let keeper = TestKeeper::new().await;
    let (signature, txn) = signed_tx();
    let original = keeper.transactions().subscribe_signature(signature).await.unwrap();

    // First append writes to the ledger; the duplicate is dropped.
    assert!(
        keeper.transactions().append(&txn).await.unwrap().is_ok(),
        "first append is accepted"
    );
    let duplicate = keeper.transactions().subscribe_signature(signature).await.unwrap();
    assert_eq!(
        keeper.transactions().append(&txn).await.unwrap(),
        Err(TransactionError::AlreadyProcessed)
    );
    assert!(matches!(
        duplicate.try_recv(),
        Err(oneshot::TryRecvError::Empty)
    ));
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
    let rejected = keeper.transactions().subscribe_signature(signature).await.unwrap();
    assert_eq!(
        keeper.transactions().append(&txn).await.unwrap(),
        Err(TransactionError::BlockhashNotFound)
    );
    assert!(matches!(
        rejected.try_recv(),
        Err(oneshot::TryRecvError::Empty)
    ));
    assert!(keeper.transactions().status(signature).await.unwrap().is_none());
    assert_eq!(
        keeper.transactions().append(&txn).await.unwrap(),
        Err(TransactionError::AlreadyProcessed)
    );

    keeper.close().await;
}
