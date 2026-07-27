#![cfg(feature = "agave-unstable-api")]
#![doc = include_str!("../README.md")]
// Parsing helpers only need to be public for benchmarks.
#[cfg(feature = "dev-context-only-utils")]
pub mod bytes;
#[cfg(not(feature = "dev-context-only-utils"))]
mod bytes;

mod address_table_lookup_frame;
mod instructions_frame;
mod message_header_frame;
pub mod resolved_transaction_view;
pub mod result;
mod sanitize;
mod signature_frame;
mod static_account_keys_frame;
mod transaction_config_frame;
pub mod transaction_data;
mod transaction_frame;
pub mod transaction_version;
pub mod transaction_view;

pub use sanitize::{
    MAGICBLOCK_INSTRUCTION_TRACE_LENGTH, MAX_MAGICBLOCK_ACCOUNT_LOCKS,
    MAX_MAGICBLOCK_TRANSACTION_SIZE, MAX_STANDARD_TRANSACTION_SIZE,
};
