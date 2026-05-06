//! Keeper integration and unit tests.
//!
//! These cover the composition layer keeper owns — startup seeding, corruption
//! recovery, the read-side caches, and subscription fanout — and deliberately
//! avoid re-testing the accountsdb/ledger internals already covered below it.

mod caches;
mod recovery;
mod subscriptions;

use crate::testkit::{TestKeeper, signed_tx};
