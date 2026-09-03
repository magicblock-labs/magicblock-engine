//! Keeper error types.

use accountsdb::{AccountsDBError, SnapshotError};
use derive_more::From;
use flume::SendError;
use ledger::{LedgerError, LedgerRequestError, request::ReadRequest, schema::Event};
use nucleus::shutdown::Service;
use solana_pubkey::Pubkey;

/// Errors produced while initializing, finalizing, or serving keeper state.
#[derive(Debug, thiserror::Error, From)]
pub enum KeeperError {
    /// Filesystem or archive IO failed.
    #[error("io: {0}")]
    IO(#[source] std::io::Error),
    /// Accounts database operation failed.
    #[error("accountsdb: {0}")]
    AccountsDB(#[source] AccountsDBError),
    /// Snapshot creation, restore, or archive operation failed.
    #[error("snapshot: {0}")]
    Snapshot(#[source] SnapshotError),
    /// Ledger initialization or append failed.
    #[error("ledger: {0}")]
    Ledger(#[source] LedgerError),
    /// A background service is no longer reachable, so the request was dropped.
    #[error("service became unavailable: {0:?}")]
    ServiceUnavailable(Service),
    /// Ledger read request failed before a response was received.
    #[error("ledger read request: {0}")]
    LedgerRequest(#[source] LedgerRequestError),
    /// A process-lifetime unicast stream was already registered.
    #[error("subscription already registered: {0}")]
    SubscriptionRegistered(&'static str),
    /// A sysvar required by persisted non-genesis state is missing.
    #[error("missing required sysvar: {0}")]
    MissingSysvar(Pubkey),
}

impl From<SendError<Event>> for KeeperError {
    fn from(_: SendError<Event>) -> Self {
        Self::ServiceUnavailable(Service::LedgerAppender)
    }
}

impl From<SendError<ReadRequest>> for KeeperError {
    fn from(_: SendError<ReadRequest>) -> Self {
        Self::ServiceUnavailable(Service::LedgerReader)
    }
}

/// Result type used by keeper APIs.
pub type Result<T> = std::result::Result<T, KeeperError>;
