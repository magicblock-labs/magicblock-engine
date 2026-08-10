//! Shared heed index plumbing.

use ::heed::{Env, Result, RoTxn, RwTxn, WithTls};

/// Read-only transaction using heed thread-local storage.
pub type RoTxnTls<'e> = RoTxn<'e, WithTls>;
/// Optional write transaction used by batched updates.
pub type OptRwTxn<'t, 'e> = &'t mut Option<RwTxn<'e>>;
/// Optional read transaction used by batched reads.
pub type OptRoTxn<'t, 'e> = &'t mut Option<RoTxnTls<'e>>;

/// Common access for heed-backed indexes.
pub trait DatabaseIndex {
    /// Returns the owning heed environment.
    fn env(&self) -> &Env;

    /// Flushes the index databases to durable storage.
    fn flush(&self) -> Result<()> {
        self.env().force_sync()
    }
}

/// Uses the supplied write transaction or opens one against `env` on demand.
pub fn write_txn<'t, 'e>(env: &'e Env, txn: OptRwTxn<'t, 'e>) -> Result<&'t mut RwTxn<'e>> {
    if let Some(txn) = txn {
        return Ok(txn);
    }
    Ok(txn.insert(env.write_txn()?))
}

/// Uses the supplied read transaction or opens one against `env` on demand.
pub fn read_txn<'t, 'e>(env: &'e Env, txn: OptRoTxn<'t, 'e>) -> Result<&'t RoTxnTls<'e>> {
    if let Some(txn) = txn {
        return Ok(txn);
    }
    Ok(txn.insert(env.read_txn()?))
}
