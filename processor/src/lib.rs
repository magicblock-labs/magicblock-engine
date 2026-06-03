#![doc = include_str!("../README.md")]

use keeper::ResolvedTransaction;
use nucleus::ledger::Block;

use crate::executor::ExecutorId;

mod callback;
mod error;
mod executor;
mod metrics;
pub mod sequencer;
pub mod simulator;
mod svm;

#[cfg(test)]
mod tests;

pub use error::{ProcessorError, Result};
pub use nucleus::runtime::{SequencerMessage, Simulation, SimulatorMessage};

/// Batch of work delivered from the sequencer to a transaction executor.
pub enum ExecutorMessage {
    /// A set of conflict-free transactions to execute together.
    Transactions(Vec<ResolvedTransaction>),
    /// A block boundary advancing the executor's processing environment.
    Block(Block),
}

/// Notification that an executor finished its batch and is idle again.
struct ExecutorReady {
    /// Identifier of the executor that became ready.
    id: ExecutorId,
    /// The drained batch handed back for reuse
    batch: Vec<ResolvedTransaction>,
}
