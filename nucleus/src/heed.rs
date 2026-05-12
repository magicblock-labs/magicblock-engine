//! Shared heed index plumbing.

use std::mem;

use ::heed::{Env, Result, RoTxn, RwTxn, WithTls};

/// Read-only transaction using heed thread-local storage.
pub type RoTxnTls<'e> = RoTxn<'e, WithTls>;
/// Optional write transaction used by batched updates.
pub type OptRwTxn<'t, 'e> = &'t mut Option<RwTxn<'e>>;
/// Optional read transaction used by batched reads.
pub type OptRoTxn<'t, 'e> = &'t mut Option<RoTxnTls<'e>>;

/// Common transaction access for heed-backed indexes.
///
/// # Safety
///
/// Implementors must return an environment owned by `self`, and callers must
/// drop transactions opened through this trait before that environment is
/// dropped. This matches the batched transaction pattern where the transaction
/// is stored in the caller's `Option` and reused across index operations.
pub unsafe trait DatabaseIndex {
    /// Returns the owning heed environment.
    fn env(&self) -> &Env;

    /// Uses the supplied write transaction or opens one on demand.
    fn write_txn<'t, 'e>(&self, txn: OptRwTxn<'t, 'e>) -> Result<&'t mut RwTxn<'e>> {
        if let Some(txn) = txn {
            return Ok(txn);
        }
        // SAFETY: guaranteed by the trait contract. The transaction is stored
        // in the caller-owned option and must be dropped before `env`.
        let write = unsafe { mem::transmute::<RwTxn<'_>, RwTxn<'e>>(self.env().write_txn()?) };
        Ok(txn.insert(write))
    }

    /// Uses the supplied read transaction or opens one on demand.
    fn read_txn<'t, 'e>(&self, txn: OptRoTxn<'t, 'e>) -> Result<&'t RoTxnTls<'e>> {
        if let Some(txn) = txn {
            return Ok(txn);
        }
        // SAFETY: guaranteed by the trait contract. The transaction is stored
        // in the caller-owned option and must be dropped before `env`.
        let read = unsafe { mem::transmute::<RoTxnTls<'_>, RoTxnTls<'e>>(self.env().read_txn()?) };
        Ok(txn.insert(read))
    }

    /// Flushes the index databases to durable storage.
    fn flush(&self) -> Result<()> {
        self.env().force_sync()
    }
}
