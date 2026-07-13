#![doc = include_str!("../README.md")]

mod client;
mod error;
mod metrics;
mod protocol;
mod server;

use std::time::Duration;

pub use client::ReplicationClient;
pub use error::ReplicationError;
pub use protocol::PROTO_VERSION;
pub use server::ReplicationDispatcher;

type Result<T> = std::result::Result<T, ReplicationError>;

/// Read/write timeout applied to replication sockets on both sides.
const IO_TIMEOUT: Duration = Duration::from_secs(4);
/// Delay a follower waits between reconnect attempts.
const RETRY_DELAY: Duration = Duration::from_secs(1);
/// Reconnect attempts a follower makes before giving up.
const MAX_RECONNECT_ATTEMPTS: usize = 10;
