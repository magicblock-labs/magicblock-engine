//! Error types shared by the ledger crate.

use std::io;

use agave_transaction_view::result::TransactionViewError;
use oneshot::RecvError;
use tokio::time::error::Elapsed;

/// Errors returned by ledger storage, codecs, and indexes.
#[derive(Debug, derive_more::From, thiserror::Error)]
pub enum LedgerError {
    /// Filesystem I/O failed while opening, writing, or flushing ledger files.
    #[error("ledger storage I/O error: {0}")]
    IO(#[source] io::Error),
    /// Platform filesystem operation failed.
    #[error("ledger filesystem operation failed: {0}")]
    FS(#[source] rustix::io::Errno),
    /// Wincode failed while serializing the blockstore stream.
    #[error("ledger blockstore codec error: {0}")]
    Wincode(#[source] wincode::Error),
    /// Bitcode failed while serializing execution details.
    #[error("ledger execution-details codec error: {0}")]
    Bitcode(#[source] bitcode::Error),
    /// Fjall failed while opening or accessing a ledger index.
    #[error("ledger index error: {0}")]
    Index(#[source] fjall::Error),
    /// Sanitized transaction view could not be decoded.
    #[error("transaction view error: {0:?}")]
    TransactionView(TransactionViewError),
    /// Ledger index pointed to an entry of the wrong type or invalid shape.
    #[error("ledger corruption: {0}")]
    #[from(skip)]
    Corruption(&'static str),
}

/// Errors returned while waiting for a ledger reader response.
#[derive(Debug, thiserror::Error)]
pub enum LedgerRequestError {
    /// Reader dropped the response channel before sending a result.
    #[error("ledger reader response channel closed: {0}")]
    ResponseFailed(#[from] RecvError),
    /// Reader did not answer within the request timeout.
    #[error("ledger reader request timed out: {0}")]
    Timeout(#[from] Elapsed),
}

/// Result type used by the ledger crate.
pub(crate) type Result<T> = std::result::Result<T, LedgerError>;
/// Result type used while waiting for asynchronous ledger reader responses.
pub(crate) type RequestResult<T> = std::result::Result<T, LedgerRequestError>;
