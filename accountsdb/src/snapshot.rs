//! Snapshot export helpers.

use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::PathBuf,
};

use crate::{ACTIVE_DIR, AccountsDB};

/// Snapshot directory prefix.
const PREFIX: &str = "snapshot-";
/// Snapshot payload filename for the volatile store.
pub(crate) const VOLATILE_DB_FILE: &str = "volatile.db";

/// Errors while writing a snapshot directory.
#[derive(thiserror::Error, Debug)]
pub enum SnapshotError {
    /// I/O while writing the snapshot.
    #[error("snapshot export I/O error")]
    IO(#[from] io::Error),
    /// Failed to flush the persisted store before copying the tree.
    #[error("failed to flush persisted store")]
    Flush(#[from] heed::Error),
    /// Failed to serialize the volatile store into the snapshot.
    #[error("failed to serialize volatile store")]
    Serde(#[from] Box<bincode::ErrorKind>),
    /// Failed to clone the active database tree into the snapshot slot.
    #[error("failed to clone snapshot tree")]
    FsClone(#[from] clonetree::Error),
}

impl AccountsDB {
    /// Writes a slot snapshot under `root`.
    ///
    /// # Safety
    /// The caller must ensure exclusive write access while the snapshot is in
    /// progress. The persisted backend is flushed first, then the active tree
    /// is cloned, and finally the volatile store is rewritten in the clone.
    /// That ordering keeps the exported state coherent only when no concurrent
    /// writes can race with the export.
    pub unsafe fn snapshot(&self, slot: u64) -> Result<PathBuf, SnapshotError> {
        let src = self.root.join(ACTIVE_DIR);
        let dst = self.root.join(format!("{PREFIX}{slot:0>12}"));
        let volatiledb = dst.join(VOLATILE_DB_FILE);
        // SAFETY: snapshot owns exclusive write access, so defrag cannot race
        // with concurrent mutation and can compact the persisted store first.
        unsafe { self.persisted.defragment() }?;
        // Persisted state must reach disk before we copy the active tree.
        self.persisted.flush(true)?;
        // Clone the whole active tree, then replace the volatile payload below.
        clonetree::clone_tree(src, &dst, &Default::default())?;

        let file = File::options().write(true).open(volatiledb)?;
        let mut buffered = BufWriter::with_capacity(1024 * 1024, file);
        // The clone may still contain a stale volatile.db; overwrite it with
        // the current in-memory volatile store.
        bincode::serialize_into(&mut buffered, &self.volatile.accounts)?;
        buffered.flush()?;

        Ok(dst)
    }
}
