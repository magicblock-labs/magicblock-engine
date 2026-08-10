//! Sequencer unit tests for account locks and executor availability.
//!
//! These tests stay below the public processor surface so they can exercise the
//! scheduling invariants directly: lock fairness, partial-acquire behavior, and
//! the bookkeeping that decides when executor work can be drained.

use std::{
    collections::VecDeque,
    sync::{Arc, mpsc},
};

use keeper::{
    Keeper,
    testkit::{TestKeeper, resolved},
};
use nucleus::shutdown::Service;
use solana_pubkey::Pubkey;
use tokio::sync::mpsc as tokio_mpsc;

use super::{
    BlockHasher, Sequencer,
    locks::AccountLock,
    pool::{AvailableExecutors, Executors},
};
use crate::{
    ExecutorMessage, ExecutorReady,
    executor::{ExecutorHandle, ExecutorId, ExecutorWork},
};

/// Constructs a bare sequencer over `tk`'s keeper without spawning executors.
///
/// Tests fill only the fields needed to call scheduling helpers directly; the
/// message channels are intentionally inert.
fn sequencer(tk: &mut TestKeeper) -> Sequencer {
    let state: Arc<Keeper> = tk.clone();
    let (_tx, rx) = tokio_mpsc::channel(1);
    let (_ready_tx, ready_rx) = tokio_mpsc::channel(1);
    let hasher = BlockHasher::new(state.blockhash());
    Sequencer {
        slot: state.blocks().current_slot(),
        state,
        locks: Default::default(),
        rx,
        hasher,
        executors: Executors::new(Vec::new(), ready_rx),
        shutdown: tk.shutdown.handle(Service::Sequencer),
        replay: false,
    }
}

/// Builds an idle executor handle plus the receiver end of its dispatch channel,
/// so a test can observe the batch `dispatch` actually sends.
fn executor_with_rx(id: ExecutorId) -> (ExecutorHandle, mpsc::Receiver<ExecutorMessage>) {
    let (tx, rx) = mpsc::sync_channel(1);
    let handle = ExecutorHandle {
        id,
        work: ExecutorWork {
            batch: Vec::new(),
            locks: Default::default(),
            blocked: VecDeque::new(),
        },
        tx,
        task: None,
    };
    (handle, rx)
}

/// [`executor_with_rx`] for tests that never inspect what was dispatched.
fn executor(id: ExecutorId) -> ExecutorHandle {
    executor_with_rx(id).0
}

// Readers share a lock until a writer contends, then the writer waits for every
// current reader and is granted before later readers.
#[test]
fn read_locks_share_until_a_writer_arrives() {
    let mut lock = AccountLock::default();

    assert_eq!(lock.read(0), Ok(()));
    assert_eq!(lock.read(1), Ok(()));
    assert_eq!(lock.write(2), Err(0));
    assert!(lock.locked());

    lock.unlock(0);
    assert_eq!(lock.write(2), Err(1));
    lock.unlock(1);
    assert_eq!(lock.write(2), Ok(()));
}

// One executor may reacquire a lock it already owns and may upgrade its own read
// lock to a write lock without blocking itself.
#[test]
fn same_executor_can_reenter_and_upgrade() {
    let mut lock = AccountLock::default();

    assert_eq!(lock.read(4), Ok(()));
    assert_eq!(lock.write(4), Ok(()));
    assert_eq!(lock.read(4), Ok(()));

    lock.unlock(4);
    assert!(!lock.locked());
    assert_eq!(lock.read(5), Ok(()));
}

// A contending executor keeps priority after it is queued, preventing unrelated
// readers from slipping ahead while the current writer drains.
#[test]
fn contender_gets_priority_until_granted() {
    let mut lock = AccountLock::default();

    assert_eq!(lock.write(0), Ok(()));
    lock.contend(1);
    assert_eq!(lock.read(2), Err(1));

    lock.unlock(0);
    assert_eq!(lock.read(1), Ok(()));
    assert_eq!(lock.read(2), Ok(()));
}

// If acquiring a multi-account transaction fails partway through, already-held
// locks keep the blocked executor marked as the contender until its turn.
#[tokio::test(flavor = "current_thread")]
async fn acquire_locks_preserves_contender_priority_after_partial_conflict() {
    let mut tk = TestKeeper::new().await;
    let mut sequencer = sequencer(&mut tk);
    let a = Pubkey::new_unique();
    let b = Pubkey::new_unique();
    let mut blocker = executor(0);
    let mut blocked = executor(1);

    sequencer.locks.acquire(&mut blocker, &resolved(&[(b, true)])).unwrap();
    let err = sequencer
        .locks
        .acquire(&mut blocked, &resolved(&[(a, true), (b, true)]))
        .expect_err("b blocks the second transaction");

    assert_eq!(err, 0);
    assert_eq!(blocked.locks.get(&a), None);
    // `a` was released after `b` conflicted, but the blocker keeps contender
    // priority so unrelated executors cannot acquire it before the retry.
    let a_lock = sequencer.locks.get_mut(&a).expect("a lock remains");
    assert!(a_lock.read(1).is_err());
    assert_eq!(a_lock.read(0), Ok(()));
    assert!(blocker.locks.contains_key(&b));
}

// Executor availability reports the first idle executor, the busy count, and the
// all-idle/all-busy states used by sequencer drain logic.
#[test]
fn available_executors_track_busy_and_idle_state() {
    let mut available = AvailableExecutors::new(3);

    assert_eq!(available.get(), Some(0));
    assert!(available.idle());
    available.remove(0);
    available.remove(2);

    assert_eq!(available.get(), Some(1));
    assert_eq!(available.busy(), 2);
    assert!(!available.empty());
    assert!(!available.idle());

    available.remove(1);
    assert!(available.empty());
    assert_eq!(available.get(), None);

    available.insert(2);
    assert_eq!(available.get(), Some(2));
    assert_eq!(available.busy(), 2);
}

// When a freed executor retries a transaction queued behind it, and that
// transaction re-acquires its now-free lock but then conflicts with a lock still
// held by another executor, it rolls back and is re-queued behind the new blocker.
#[tokio::test(flavor = "current_thread")]
async fn handle_ready_requeues_behind_the_new_blocker() {
    let mut tk = TestKeeper::new().await;
    let mut sequencer = sequencer(&mut tk);
    let x = Pubkey::new_unique();
    let y = Pubkey::new_unique();

    let mut e0 = executor(0);
    let mut e1 = executor(1);
    sequencer.locks.acquire(&mut e0, &resolved(&[(x, true)])).unwrap();
    sequencer.locks.acquire(&mut e1, &resolved(&[(y, true)])).unwrap();
    // A transaction needing both x and y is parked behind executor 0, the x holder.
    e0.blocked.push_back(resolved(&[(x, true), (y, true)]));

    let (_ready_tx, ready_rx) = tokio_mpsc::channel(1);
    sequencer.executors = Executors::new(vec![e0, e1], ready_rx);

    // Executor 0 finishes: x is released, but the retried transaction now conflicts
    // with y (still held by executor 1) and is re-parked behind executor 1.
    sequencer.handle_ready(ExecutorReady { id: 0, batch: Vec::new() }).unwrap();

    assert_eq!(
        sequencer.executors.handles[1].blocked.len(),
        1,
        "requeued behind y's holder"
    );
    assert!(
        sequencer.executors.handles[0].blocked.is_empty(),
        "no longer queued behind x"
    );
    assert!(
        sequencer.executors.handles[0].batch.is_empty(),
        "nothing dispatched to executor 0"
    );
    // x was rolled back (no holder) but keeps executor 1 as its priority contender.
    let x_lock = sequencer.locks.get_mut(&x).expect("x lock retained");
    assert!(!x_lock.locked());
    assert_eq!(
        x_lock.read(2),
        Err(1),
        "blocker keeps contender priority on x"
    );
    // y is still write-held by executor 1.
    assert_eq!(
        sequencer.locks.get_mut(&y).expect("y lock retained").read(2),
        Err(1)
    );
}

// A freed executor re-dispatches a transaction queued behind it once it can fully
// re-acquire its locks, handing it back to the executor in a fresh batch.
#[tokio::test(flavor = "current_thread")]
async fn handle_ready_redispatches_unblocked_transaction() {
    let mut tk = TestKeeper::new().await;
    let mut sequencer = sequencer(&mut tk);
    let x = Pubkey::new_unique();

    let (mut e0, rx) = executor_with_rx(0);
    sequencer.locks.acquire(&mut e0, &resolved(&[(x, true)])).unwrap();
    e0.blocked.push_back(resolved(&[(x, true)]));

    let (_ready_tx, ready_rx) = tokio_mpsc::channel(1);
    sequencer.executors = Executors::new(vec![e0], ready_rx);

    // Executor 0 finishes: x is released, the queued transaction re-acquires it and
    // is dispatched straight back to executor 0.
    sequencer.handle_ready(ExecutorReady { id: 0, batch: Vec::new() }).unwrap();

    let ExecutorMessage::Transactions(batch) = rx.try_recv().expect("batch dispatched") else {
        panic!("dispatched a transaction batch");
    };
    assert_eq!(batch.len(), 1);
    assert!(batch[0].static_account_keys().contains(&x));
    // x is held again for the redispatched transaction, and its queue is empty.
    assert_eq!(
        sequencer.locks.get_mut(&x).expect("x lock").read(2),
        Err(0),
        "x re-held by executor 0"
    );
    assert!(sequencer.executors.handles[0].blocked.is_empty());
}
