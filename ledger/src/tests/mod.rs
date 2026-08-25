//! Ledger test modules.
//!
//! `codec` covers serialization and compression, `index` covers Fjall storage,
//! and `integration` drives the append→seal→read pipeline end to end through
//! the appender and reader.

mod codec;
mod index;
mod integration;
