#![doc = include_str!("../README.md")]

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
