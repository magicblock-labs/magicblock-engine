//! Executor-pool availability, dispatch, and lifecycle bookkeeping.

use nucleus::ledger::Block;
use tokio::sync::mpsc::Receiver;

use super::ReadyTransaction;
use crate::{
    Result,
    executor::{ExecutorEvent, ExecutorHandle, ExecutorId, ExecutorMessage},
    metrics,
};

/// Maximum executor count represented by the availability bitset.
pub(super) const MAX_EXECUTORS: u32 = u64::BITS;

/// Executor handles and one availability bit per contiguous executor ID.
pub(super) struct Executors {
    /// Worker handles indexed by their executor IDs.
    handles: Vec<ExecutorHandle>,
    /// Channel on which workers report transaction completion or failure.
    pub(super) events: Receiver<ExecutorEvent>,
    /// Set bits identify executors available for immediate dispatch.
    available: u64,
}

impl Executors {
    /// Builds an idle pool from handles with contiguous IDs starting at zero.
    pub(super) fn new(handles: Vec<ExecutorHandle>, events: Receiver<ExecutorEvent>) -> Self {
        let count = handles.len() as u32;
        let available = if count == 0 { 0 } else { u64::MAX >> (u64::BITS - count) };
        Self { handles, events, available }
    }

    /// Number of executors in the pool.
    pub(super) fn capacity(&self) -> usize {
        self.handles.len()
    }

    /// First available executor, if all workers are not already busy.
    pub(super) fn available(&self) -> Option<ExecutorId> {
        let id = self.available.trailing_zeros();
        (id != u64::BITS).then_some(id)
    }

    /// Dispatches one dependency-free transaction to an available executor.
    pub(super) fn dispatch(&mut self, id: ExecutorId, ready: ReadyTransaction) -> Result<()> {
        debug_assert_ne!(self.available & (1 << id), 0);
        self.handles[id as usize].send(ExecutorMessage::Transaction(ready))?;
        self.available &= !(1 << id);
        self.update_metrics();
        Ok(())
    }

    /// Returns a worker to the available set after its trusted completion signal.
    pub(super) fn release(&mut self, id: ExecutorId) {
        debug_assert_eq!(self.available & (1 << id), 0);
        self.available |= 1 << id;
        self.update_metrics();
    }

    /// Advances every idle worker to a new block environment.
    pub(super) fn transition(&self, block: Block) -> Result<()> {
        for executor in &self.handles {
            executor.send(ExecutorMessage::Block(block))?;
        }
        Ok(())
    }

    /// Closes worker channels and joins every executor thread.
    pub(super) fn join(&mut self) {
        for executor in self.handles.drain(..) {
            executor.join();
        }
    }

    /// Refreshes the busy gauge from the pool's sole availability state.
    fn update_metrics(&self) {
        metrics::busy_executors(self.handles.len() - self.available.count_ones() as usize);
    }
}
