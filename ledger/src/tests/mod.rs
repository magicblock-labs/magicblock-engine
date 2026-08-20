//! Ledger test modules.
//!
//! `index` covers the Fjall codec/index in isolation; `integration` drives the
//! append→seal→read pipeline end to end through the appender and reader.

mod index;
mod integration;
