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
    /// Removes temporary snapshot exports left by a previous process.
    ///
    /// Call during startup, before any snapshot archiver can use these paths.
    /// The root must contain no unrelated `snapshot-*` entries.
    pub fn cleanup_snapshots(&self) -> SnapshotResult<()> {
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_name().to_str().is_some_and(|name| name.starts_with(PREFIX)) {
                fs::remove_dir_all(entry.path())?;
            }
        }
        Ok(())
    }

    /// Writes a superblock snapshot under `root`.
    ///
    /// # Safety
    /// The caller must exclude account writes for the duration of export and
    /// quiesce any raw borrowed views not covered by reader scopes.
    /// Reader admission is paused internally during the packing pass;
    /// ordinary reads may run again while the flushed tree is cloned.
    pub unsafe fn snapshot(&self, superblock: u64) -> SnapshotResult<PathBuf> {
        let _timer = metrics::time(Operation::Snapshot);
        let src = self.root.join(ACTIVE_DIR);
        let dst = self.root.join(format!("{PREFIX}{superblock:0>9}"));
        self.set_superblock(superblock);
        {
            let _pause = self.readers.pause()?;
            // SAFETY: the caller excludes writers; the pause drains readers
            // and prevents new index snapshots until relocation is complete.
            unsafe { self.persisted.defragment() }?;
        }
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

    /// Saves or restores the active database tree and returns its destination.
    ///
    /// After restoring, callers must drop this instance and reopen the database:
    /// its open handles still refer to the removed active tree.
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
        fs::rename(from, to)?;
        Ok(to.clone())
    }
}
