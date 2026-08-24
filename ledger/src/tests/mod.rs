//! Ledger test modules.
//!
//! `codec` and `index` cover their storage components in isolation;
//! `integration` drives the append→seal→read pipeline end to end through the
//! appender and reader.

mod codec;
mod index;
mod integration;
