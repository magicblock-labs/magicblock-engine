use std::io;

use derive_more::From;
use engine::EngineError;
use keeper::error::KeeperError;
use ledger::LedgerError;
use nucleus::ledger::BlockstorePosition;
use tokio::time::error::Elapsed;

/// Failure while negotiating or transferring replicated state.
#[derive(From, thiserror::Error, Debug)]
pub enum ReplicationError {
    /// Socket or replicated-file access failed.
    #[error("replication I/O failed: {0}")]
    IO(#[source] io::Error),
    /// Applying replicated state through the keeper failed.
    #[error("failed to apply replicated state: {0}")]
    State(#[source] KeeperError),
    /// Applying a replicated entry through the execution engine failed.
    #[error("replication engine operation failed: {0}")]
    Engine(#[source] EngineError),
    /// Reading or advancing replicated ledger storage failed.
    #[error("replication ledger operation failed: {0}")]
    Ledger(#[source] LedgerError),
    /// A control message could not be encoded or decoded.
    #[error("invalid replication control message: {0}")]
    Serde(#[source] wincode::Error),
    /// The peer uses a protocol version this crate cannot read.
    #[error("replication protocol version mismatch; expected version {0}")]
    VersionMismatch(u32),
    /// The requested or published blockstore cursor is unavailable locally.
    #[error("replication position is unavailable: {0:?}")]
    PositionNotFound(BlockstorePosition),
    /// The leader rejected the client's handshake.
    #[error("replication handshake rejected: {0}")]
    Handshake(String),
    /// The snapshot connection ended before the advertised byte count arrived.
    #[error("incomplete replication snapshot: expected {0} bytes, received {1}")]
    Snapshot(u64, u64),
    /// No complete retained snapshot can satisfy an unavailable cursor.
    #[error("no complete replication snapshot is available")]
    SnapshotUnavailable,
    /// All bounded attempts to reconnect to the leader failed.
    #[error("replication reconnect attempts exhausted")]
    ReconnectExhausted,
    /// A staged snapshot must be installed by restarting the engine.
    #[error("replication snapshot for superblock {0} is staged; restart required")]
    RestartRequired(u64),
    /// A replication event stream closed before the transfer completed.
    #[error("replication event stream closed")]
    StreamClosed,
    /// Waiting for a locally committed block boundary timed out.
    #[error("timed out waiting for a replicated block boundary: {0}")]
    Timeout(#[source] Elapsed),
}
