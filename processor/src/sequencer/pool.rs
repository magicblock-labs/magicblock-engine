//! Executor-pool bookkeeping for the sequencer.

use std::mem;

use keeper::ResolvedTransaction;
use nucleus::shutdown::Service;
use tokio::sync::mpsc::Receiver;
use tracing::warn;

use super::MAX_BLOCKED_EXECUTOR_TXNS;
use crate::{
    ExecutorMessage, ExecutorReady, Result,
    executor::{ExecutorHandle, ExecutorId},
    metrics,
};

/// The pool of executors and the state needed to dispatch work to them.
pub(super) struct Executors {
    /// One handle per executor worker, indexed by [`ExecutorId`].
    pub(super) handles: Vec<ExecutorHandle>,
    /// Channel on which executors signal they have finished a batch.
    pub(super) ready: Receiver<ExecutorReady>,
    /// Bitset of executors currently free to accept a batch.
    available: AvailableExecutors,
}

/// Bitset of free executors, one bit per [`ExecutorId`].
pub(super) struct AvailableExecutors {
    /// Set bits are executor IDs currently free to accept work.
    bitflags: u64,
    /// Number of executor slots represented by `bitflags`.
    total: u32,
}

impl Executors {
    /// Builds the pool from spawned executor handles and their readiness channel.
    pub(super) fn new(handles: Vec<ExecutorHandle>, ready: Receiver<ExecutorReady>) -> Self {
        let available = AvailableExecutors::new(handles.len() as u32);
        Self { handles, ready, available }
    }

    /// Whether the pool can accept more work: at least one executor is free and
    /// no executor's blocked queue has reached the backpressure threshold.
    pub(super) fn ready(&self) -> bool {
        let saturated = |h: &ExecutorHandle| h.blocked.len() >= MAX_BLOCKED_EXECUTOR_TXNS;
        !(self.available.empty() || self.handles.iter().any(saturated))
    }

    /// Returns whether every executor is currently free.
    pub(super) fn idle(&self) -> bool {
        self.available.idle()
    }

    /// Queues a transaction behind the executor that currently blocks it, to be
    /// retried once that executor releases its conflicting locks.
    pub(super) fn enqueue(&mut self, txn: ResolvedTransaction, executor: ExecutorId) {
        self.handles[executor as usize].blocked.push_back(txn);
        metrics::blocked_transaction();
    }

    /// Returns a handle to a currently free executor, or `None` if all are busy.
    /// The executor is not yet marked busy; the caller does that once it commits
    /// a batch to it.
    pub(super) fn available(&mut self) -> Option<&mut ExecutorHandle> {
        self.available.get().and_then(|idx| self.get(idx))
    }

    /// Returns the handle for executor `idx`, if such an executor exists.
    fn get(&mut self, idx: ExecutorId) -> Option<&mut ExecutorHandle> {
        self.handles.get_mut(idx as usize)
    }

    /// Marks executor `idx` as available again and returns its handle.
    pub(super) fn release(&mut self, idx: ExecutorId) -> Option<&mut ExecutorHandle> {
        self.available.insert(idx);
        metrics::busy_executors(self.available.busy());
        self.get(idx)
    }

    /// Sends executor `idx`'s accumulated batch to its worker and marks it busy.
    /// A no-op if the executor is unknown or its batch is empty.
    pub(super) fn dispatch(&mut self, idx: ExecutorId) -> Result<()> {
        let Some(executor) = self.get(idx) else {
            warn!(idx, "dispatch to unknown executor; ignoring");
            return Ok(());
        };
        if executor.batch.is_empty() {
            return Ok(());
        }
        let msg = ExecutorMessage::Transactions(mem::take(&mut executor.batch));
        executor.tx.send(msg).map_err(|_| Service::TransactionExecutor(idx))?;
        self.available.remove(idx);
        metrics::busy_executors(self.available.busy());
        Ok(())
    }
}

impl AvailableExecutors {
    /// Starts with every executor marked available.
    pub(super) fn new(executors: u32) -> Self {
        Self {
            bitflags: (1u64 << executors) - 1,
            total: executors,
        }
    }

    /// Returns the id of an available executor, or `None` if all are busy.
    pub(super) fn get(&self) -> Option<ExecutorId> {
        let position = self.bitflags.trailing_zeros();
        (position != u64::BITS).then_some(position)
    }

    /// Returns whether no executor is currently available.
    pub(super) fn empty(&self) -> bool {
        self.bitflags == 0
    }

    /// Returns whether every executor is currently free.
    pub(super) fn idle(&self) -> bool {
        self.bitflags.count_ones() == self.total
    }

    /// Returns how many executors are currently busy.
    pub(super) fn busy(&self) -> usize {
        (self.total - self.bitflags.count_ones()) as usize
    }

    /// Marks an executor as busy.
    pub(super) fn remove(&mut self, executor: ExecutorId) {
        self.bitflags &= !(1 << executor)
    }

    /// Marks an executor as available again.
    pub(super) fn insert(&mut self, executor: ExecutorId) {
        self.bitflags |= 1 << executor
    }
}
