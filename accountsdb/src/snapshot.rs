//! Snapshot export helpers.

use std::{
    fs::{self, File},
    io::{self, BufWriter},
    path::PathBuf,
};

use nucleus::MB;
use tracing::info;

use crate::{
    ACTIVE_DIR, AccountsDB,
    metrics::{self, Operation},
};

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
    FsClone(#[from] Box<clonetree::Error>),
    /// No archived snapshot could be restored.
    #[error("no valid archived accountsdb snapshot found")]
    Missing,
}

/// Result type used by snapshot export and restore helpers.
pub type SnapshotResult<T> = Result<T, SnapshotError>;

/// Active database backup operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackupOp {
    /// Move the active database tree to its backup path.
    Save,
    /// Move the saved backup tree back to the active database path.
    Restore,
}

impl AccountsDB {
    /// Writes a superblock snapshot under `root`.
    ///
    /// # Safety
    /// The caller must ensure exclusive write access while the snapshot is in
    /// progress. The persisted backend is flushed first, then the active tree
    /// is cloned, and finally the volatile store is rewritten in the clone.
    /// That ordering keeps the exported state coherent only when no concurrent
    /// writes can race with the export.
    pub unsafe fn snapshot(&self, superblock: u64) -> SnapshotResult<PathBuf> {
        let _timer = metrics::time(Operation::Snapshot);
        let src = self.root.join(ACTIVE_DIR);
        let dst = self.root.join(format!("{PREFIX}{superblock:0>9}"));
        self.set_superblock(superblock);
        // SAFETY: snapshot owns exclusive write access, so defrag cannot race
        // with concurrent mutation and can compact the persisted store first.
        unsafe { self.persisted.defragment() }?;
        // Persisted state must reach disk before we copy the active tree.
        self.persisted.flush(true)?;
        // Clone the whole active tree, then replace the volatile payload below.
        clonetree::clone_tree(src, &dst, &Default::default()).map_err(Box::new)?;
        self.dump(Some(&dst))?;

        Ok(dst)
    }

    /// Serializes volatile accounts into `volatile.db` under `dst`.
    ///
    /// When `dst` is omitted, writes into the active database tree so the next
    /// open restores the volatile store and consumes the file. Callers must
    /// prevent concurrent account writes to obtain a coherent image.
    pub fn dump(&self, dst: Option<&PathBuf>) -> SnapshotResult<()> {
        let _timer = metrics::time(Operation::Dump);
        let path = match dst {
            Some(dst) => dst.join(VOLATILE_DB_FILE),
            None => Self::directory(&self.root).join(VOLATILE_DB_FILE),
        };
        let db = File::options().create(true).truncate(true).write(true).open(path)?;
        let mut buffered = BufWriter::with_capacity(4 * MB, db);
        bincode::serialize_into(&mut buffered, &self.volatile.accounts)?;
        let db = buffered.into_inner().map_err(|e| e.into_error())?;
        db.sync_data().map_err(Into::into)
    }

    /// Saves or restores the active database tree.
    pub fn backup(&self, op: BackupOp) -> SnapshotResult<PathBuf> {
        let active = self.root.join(ACTIVE_DIR);
        let backup = self.root.join(format!("{ACTIVE_DIR}.bkp"));
        let (from, to) = match op {
            BackupOp::Save => (&active, &backup),
            BackupOp::Restore => (&backup, &active),
        };
        if to.exists() {
            fs::remove_dir_all(to)?;
        }
        info!(?op, "accountsdb backup");
        fs::rename(from, to).map(|_| backup).map_err(Into::into)
    }
}
