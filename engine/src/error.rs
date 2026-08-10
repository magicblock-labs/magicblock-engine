//! Engine error types.

use agave_transaction_view::result::TransactionViewError;
use derive_more::From;
use keeper::error::KeeperError;
use ledger::{LedgerError, LedgerRequestError};
use nucleus::shutdown::Service;
use processor::ProcessorError;
use solana_message::CompileError;
use solana_transaction::{InstructionError, SignerError, TransactionError};
use tokio::sync::mpsc::error::SendError;

/// Result type used by engine APIs.
pub type Result<T> = std::result::Result<T, EngineError>;

/// Failures surfaced by the top-level engine.
#[derive(From, thiserror::Error, Debug)]
pub enum EngineError {
    /// A durable-state (keeper) operation failed.
    #[error("state error: {0}")]
    State(#[source] KeeperError),
    /// Scheduling or executing a transaction failed.
    #[error("processor error: {0}")]
    Processor(#[source] ProcessorError),
    /// Replaying the ledger into volatile state on startup failed.
    #[error("replay error: {0}")]
    Replay(#[source] ReplayError),
    /// A background service is no longer reachable.
    #[error("service became unavailable: {0:?}")]
    ServiceUnavailable(Service),
    /// The engine has begun coordinated shutdown and rejects new work.
    #[error("engine is shutting down")]
    ShuttingDown,
    /// Timed out waiting for a submitted transaction's committed result.
    #[error("timed out waiting for transaction result")]
    TransactionTimeout,
    /// Signing a transaction with the engine authority failed.
    #[error("signature error: {0}")]
    Signature(#[source] SignerError),
    /// Serializing or deserializing a transaction failed.
    #[error("serialization error: {0}")]
    Serde(#[source] wincode::Error),
    /// Sanitizing a serialized transaction into a transaction view failed.
    #[error("transaction sanitization: {0:?}")]
    Sanitization(TransactionViewError),
    /// Compiling instructions into a versioned transaction message failed.
    #[error("transaction compilation failed: {0}")]
    TransactionCompile(#[source] CompileError),
    /// A submitted transaction carried an invalid signature.
    #[error("transaction signature verification failed")]
    SignatureVerification,
    /// A submitted transaction was committed with an execution failure.
    #[error("transaction execution failed: {0}")]
    TransactionExecution(#[source] TransactionError),
    /// An unexpected internal failure carrying a contextual message.
    #[error("internal error: {0}")]
    Internal(String),
}

impl<T> From<SendError<T>> for EngineError {
    fn from(_: SendError<T>) -> Self {
        Self::ServiceUnavailable(Service::Sequencer)
    }
}
impl From<InstructionError> for EngineError {
    fn from(error: InstructionError) -> Self {
        Self::TransactionExecution(TransactionError::InstructionError(0, error))
    }
}
impl From<oneshot::RecvError> for EngineError {
    fn from(_: oneshot::RecvError) -> Self {
        Self::ServiceUnavailable(Service::Sequencer)
    }
}

/// Failures raised while replaying retained ledger entries on startup.
#[derive(From, thiserror::Error, Debug)]
pub enum ReplayError {
    /// A retained transaction could not be sanitized into a transaction view.
    #[error("transaction sanitization: {0:?}")]
    Sanitization(TransactionViewError),
    /// The replayed account state checksum diverged from the sealed superblock.
    #[error("replayed state checksum mismatch")]
    StateMismatch,
    /// Waiting for the ledger reader's replay response failed.
    #[error("ledger replay request failed: {0}")]
    Request(#[source] LedgerRequestError),
    /// Reading or decoding retained ledger entries failed.
    #[error("ledger replay failed: {0}")]
    Ledger(#[source] LedgerError),
}
