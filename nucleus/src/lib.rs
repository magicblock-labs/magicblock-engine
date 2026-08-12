#![doc = include_str!("../README.md")]

use std::time::{Duration, UNIX_EPOCH};

#[cfg(feature = "config")]
pub mod config;

#[cfg(feature = "heed")]
pub mod heed;

#[cfg(feature = "shutdown")]
pub mod shutdown;

#[cfg(feature = "notifier")]
pub mod notifier;

#[cfg(feature = "ledger")]
pub mod ledger;

#[cfg(feature = "metrics")]
pub mod metrics;

#[cfg(feature = "runtime")]
pub mod runtime;

#[cfg(feature = "testkit")]
pub mod testkit;

#[cfg(feature = "tls")]
pub mod tls;

/// Ledger slot number.
pub type Slot = u64;
/// One kibibyte in bytes.
pub const KB: usize = 1024;
/// One mebibyte in bytes.
pub const MB: usize = 1024 * KB;
/// One gibibyte in bytes.
pub const GB: usize = 1024 * MB;

/// Returns the duration since the Unix epoch, or zero if the clock predates it.
pub fn unix_time() -> Duration {
    UNIX_EPOCH.elapsed().unwrap_or_default()
}
