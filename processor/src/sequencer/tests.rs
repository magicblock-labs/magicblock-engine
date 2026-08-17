//! Sequencer unit tests for deterministic dependency ordering and pool draining.

use std::sync::{Arc, mpsc};

use keeper::{
    Keeper,
    testkit::{TestKeeper, resolved},
};
use nucleus::shutdown::Service;
use solana_pubkey::Pubkey;
use tokio::sync::mpsc as tokio_mpsc;

use super::{
    BlockHasher, MAX_PENDING_EXECUTOR_TXNS, Sequencer, order::OrderingTable, pool::Executors,
};
use crate::executor::{ExecutorEvent, ExecutorHandle, ExecutorId, ExecutorMessage};

fn sequencer(tk: &mut TestKeeper) -> Sequencer {
    let state: Arc<Keeper> = tk.clone();
    let (_tx, rx) = tokio_mpsc::channel(1);
    let (_event_tx, event_rx) = tokio_mpsc::channel(1);
    Sequencer {
        slot: state.blocks().current_slot(),
        hasher: BlockHasher::new(state.blockhash()),
        state,
        ordering: Default::default(),
        rx,
        executors: Executors::new(Vec::new(), event_rx),
        shutdown: tk.shutdown.handle(Service::Sequencer),
        replay: false,
    }
}

fn executor_with_rx(id: ExecutorId) -> (ExecutorHandle, mpsc::Receiver<ExecutorMessage>) {
    ExecutorHandle::mock(id)
}

fn take_ticket(ordering: &mut OrderingTable) -> usize {
    ordering.take_ready().expect("transaction is ready").ticket
}

/// Proves leading readers may run together, a following writer waits for both
/// in either completion order, and a later reader waits for that writer.
#[test]
fn readers_share_and_writer_excludes_in_completion_order() {
    let account = Pubkey::new_unique();
    let mut ordering = OrderingTable::default();

    assert!(!ordering.register(resolved(&[(account, false)])));
    assert!(!ordering.register(resolved(&[(account, false)])));
    assert!(ordering.register(resolved(&[(account, true)])));
    assert!(ordering.register(resolved(&[(account, false)])));

    assert_eq!(take_ticket(&mut ordering), 0);
    assert_eq!(take_ticket(&mut ordering), 1);
    assert!(ordering.take_ready().is_none());
    assert_eq!(ordering.complete(1), 0);
    assert_eq!(ordering.complete(0), 1);
    assert_eq!(take_ticket(&mut ordering), 2);
    assert!(ordering.take_ready().is_none());
    assert_eq!(ordering.complete(2), 1);
    assert_eq!(take_ticket(&mut ordering), 3);
}

/// Proves the X(A) -> Y(A,B) -> Z(B) regression always releases Y before Z.
#[test]
fn multi_account_dependency_preserves_stream_order() {
    let a = Pubkey::new_unique();
    let b = Pubkey::new_unique();
    let mut ordering = OrderingTable::default();

    ordering.register(resolved(&[(a, true)]));
    ordering.register(resolved(&[(a, true), (b, true)]));
    ordering.register(resolved(&[(b, true)]));

    assert_eq!(take_ticket(&mut ordering), 0);
    assert!(ordering.take_ready().is_none());
    assert_eq!(ordering.complete(0), 1);
    assert_eq!(take_ticket(&mut ordering), 1);
    assert!(ordering.take_ready().is_none());
    assert_eq!(ordering.complete(1), 1);
    assert_eq!(take_ticket(&mut ordering), 2);
}

fn bridge_order(first: usize, second: usize) -> Vec<usize> {
    let a = Pubkey::new_unique();
    let b = Pubkey::new_unique();
    let mut ordering = OrderingTable::default();
    ordering.register(resolved(&[(a, true)]));
    ordering.register(resolved(&[(b, true)]));
    ordering.register(resolved(&[(a, true), (b, true)]));
    ordering.register(resolved(&[(a, false)]));
    ordering.register(resolved(&[(b, true)]));

    assert_eq!(take_ticket(&mut ordering), 0);
    assert_eq!(take_ticket(&mut ordering), 1);
    assert_eq!(ordering.complete(first), 0);
    assert_eq!(ordering.complete(second), 1);
    let mut released = vec![take_ticket(&mut ordering)];
    assert_eq!(ordering.complete(2), 2);
    released.push(take_ticket(&mut ordering));
    released.push(take_ticket(&mut ordering));
    released
}

/// Proves alternate executor completion orders produce the same transitive
/// conflict order for a multi-account bridge and its dependents.
#[test]
fn executor_completion_order_does_not_change_conflict_order() {
    assert_eq!(bridge_order(0, 1), vec![2, 3, 4]);
    assert_eq!(bridge_order(1, 0), vec![2, 3, 4]);
}

/// Proves ready work fills the pool, pending work applies exact backpressure, a
/// completed drain resets dependency state, and executor failure releases none.
#[tokio::test(flavor = "current_thread")]
async fn pool_backpressure_and_drain_reset_dependency_state() {
    let mut tk = TestKeeper::new().await;
    let mut sequencer = sequencer(&mut tk);
    let (e0, rx0) = executor_with_rx(0);
    let (e1, rx1) = executor_with_rx(1);
    let (_event_tx, event_rx) = tokio_mpsc::channel(1);
    sequencer.executors = Executors::new(vec![e0, e1], event_rx);

    for _ in 0..MAX_PENDING_EXECUTOR_TXNS * 2 {
        sequencer.ordering.register(resolved(&[(Pubkey::new_unique(), true)]));
    }
    assert!(!sequencer.accepting());
    sequencer.dispatch_ready().unwrap();
    assert!(matches!(
        rx0.try_recv(),
        Ok(ExecutorMessage::Transaction { .. })
    ));
    assert!(matches!(
        rx1.try_recv(),
        Ok(ExecutorMessage::Transaction { .. })
    ));

    // Complete all tickets while consuming each singular dispatch so the fake
    // executor channels model workers becoming ready again.
    for ticket in 0..MAX_PENDING_EXECUTOR_TXNS * 2 {
        let id = (ticket % 2) as ExecutorId;
        sequencer.handle_event(ExecutorEvent::Completed { id, ticket }).unwrap();
        if ticket + 2 < MAX_PENDING_EXECUTOR_TXNS * 2 {
            let rx = if id == 0 { &rx0 } else { &rx1 };
            assert!(matches!(
                rx.try_recv(),
                Ok(ExecutorMessage::Transaction { .. })
            ));
        }
    }
    sequencer.drain().await.unwrap();
    assert!(sequencer.ordering.is_empty());
    assert!(sequencer.accepting());

    let account = Pubkey::new_unique();
    assert!(!sequencer.ordering.register(resolved(&[(account, true)])));
    assert!(sequencer.ordering.register(resolved(&[(account, true)])));
    sequencer.dispatch_ready().unwrap();
    let Ok(ExecutorMessage::Transaction(ready)) = rx0.try_recv() else {
        panic!("first transaction was not dispatched after drain");
    };
    assert_eq!(ready.ticket, 0, "tickets reset at drain");
    assert!(matches!(rx1.try_recv(), Err(mpsc::TryRecvError::Empty)));

    let error = sequencer
        .handle_event(ExecutorEvent::Failed { id: 0 })
        .expect_err("executor failure must fail-stop the sequencer");
    assert!(matches!(
        error,
        crate::ProcessorError::ServiceUnavailable(Service::TransactionExecutor(0))
    ));
    assert_eq!(
        sequencer.ordering.len(),
        2,
        "failed ticket remains outstanding"
    );
    assert!(sequencer.ordering.take_ready().is_none());
    assert!(matches!(rx1.try_recv(), Err(mpsc::TryRecvError::Empty)));
}
