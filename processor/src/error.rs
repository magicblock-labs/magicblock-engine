//! Error type for the transaction processor.

use std::io;

use derive_more::From;
use keeper::error::KeeperError;
use nucleus::shutdown::Service;

/// Convenience result alias for processor operations.
pub type Result<T> = std::result::Result<T, ProcessorError>;

/// Failures raised while scheduling or executing transactions.
#[derive(Debug, thiserror::Error, From)]
pub enum ProcessorError {
    /// An I/O error, e.g. while spawning a worker thread or building a runtime.
    #[error("i/o error: {0}")]
    Io(#[source] io::Error),
    /// A failure surfaced by the underlying keeper-backed state.
    #[error("state error: {0}")]
    State(#[source] KeeperError),
    /// A background service is no longer reachable, so work could not be handed off.
    #[error("service became unavailable: {0:?}")]
    ServiceUnavailable(Service),
    /// An internal invariant was violated; carries a human-readable description.
    #[error("internal error: {0}")]
    Internal(String),
}
