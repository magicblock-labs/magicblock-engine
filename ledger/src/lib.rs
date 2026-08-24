#![doc = include_str!("../README.md")]

use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering::*},
    thread::{self, JoinHandle},
};

pub use crate::error::{LedgerError, LedgerRequestError};
use derive_more::Deref;
use fjall::Keyspace;
use nucleus::{
    Slot,
    ledger::BlockstorePosition,
    shutdown::{Service, ShutdownManager, ShutdownReason},
};
use parking_lot::RwLock;
use tokio::sync::broadcast;
use tracing::info;

mod appender;
mod codec;
mod error;
mod index;
mod metrics;
mod reader;
pub mod request;
pub mod schema;
mod storage;

#[cfg(test)]
mod tests;

use crate::{
    appender::{BLOCKSTORE_DB, EXECUTIONS_DB, LedgerAppender, SUPERBLOCK_META},
    error::Result,
    index::Index,
    reader::LedgerReader,
    request::ReaderSender,
    schema::Event,
    storage::{LedgerMeta, MetaMap, SuperblockMeta},
};

const LEDGER_META: &str = "ledger.meta";
const APPENDER_QUEUE_CAPACITY: usize = 2_048;
const READER_QUEUE_CAPACITY: usize = 128;
/// Current on-disk superblock format version.
const VERSION: LedgerVersion = 1;
/// Version tag stored at the start of every superblock metadata header.
pub type LedgerVersion = u64;

/// Top-level ledger handle.
///
/// `head` in ledger metadata is the active superblock id. Older superblocks
/// stay open in `superblocks` until retention removes their directories.
pub struct Ledger {
    /// Mmap-backed ledger metadata shared with the appender.
    meta: MetaMap<LedgerMeta>,
    /// Retained superblocks keyed by id.
    superblocks: RwLock<BTreeMap<u64, Arc<Superblock>>>,
    /// Ledger-wide index partitioned by superblock keyspace.
    index: Index,
    /// Root ledger directory.
    pub directory: PathBuf,
    /// Maximum used bytes allowed on the ledger filesystem before retention runs.
    size_limit: u64,
}

impl Ledger {
    /// Opens ledger files and the global index, then starts the appender and reader pool.
    pub fn init(
        directory: impl AsRef<Path>,
        size_limit: u64,
        shutdown: &mut ShutdownManager,
    ) -> Result<LedgerHandle> {
        let directory = directory.as_ref().to_owned();
        let ledger = Arc::new(Self::new(directory, size_limit)?);
        metrics::init(&ledger);
        let (appender_tx, rx) = flume::bounded(APPENDER_QUEUE_CAPACITY);
        let (position, _) = broadcast::channel(256);
        let mut sh = shutdown.handle(Service::LedgerAppender);
        let appender_ledger = ledger.clone();
        let appender_position = position.clone();
        thread::Builder::new().name("ledger-appender".into()).spawn(move || {
            let reason = match LedgerAppender::run(appender_ledger, rx, appender_position) {
                Ok(()) => ShutdownReason::Signalled,
                Err(error) => ShutdownReason::Error(Box::new(error)),
            };
            sh.terminate(reason);
        })?;
        let (reader_tx, rx) = flume::bounded(READER_QUEUE_CAPACITY);

        #[cfg(not(feature = "testkit"))]
        let readers = num_cpus::get() as u32;
        #[cfg(feature = "testkit")]
        let readers = 1;

        for id in 0..readers {
            let sh = shutdown.handle(Service::LedgerReader);
            let reader = LedgerReader::new(ledger.clone(), rx.clone())?;
            thread::Builder::new()
                .name(format!("ledger-reader-{id}"))
                .spawn(|| reader.run(sh))?;
        }
        info!(readers, "initialized ledger");
        Ok(LedgerHandle {
            ledger,
            reader: reader_tx,
            appender: appender_tx,
            position,
        })
    }

    /// Iterates retained superblocks from newest to oldest.
    pub fn iter(&self) -> impl Iterator<Item = Arc<Superblock>> + '_ {
        let range = self.meta.superblocks();
        range.rev().filter_map(|id| self.superblocks.read().get(&id).cloned())
    }

    /// Returns the highest slot recorded across retained superblocks, or
    /// `None` when none are retained. Taken over all superblocks rather than
    /// just the head: right after a seal the active head has no blocks yet and
    /// its persisted range is still zeroed.
    pub fn tip(&self) -> Option<Slot> {
        self.iter().map(|s| s.meta.range.end.load(Acquire)).max()
    }

    /// Blockstore write offset of a retained superblock, `None` when it is not retained.
    pub fn cursor(&self, superblock: u64) -> Option<u64> {
        if !self.meta.superblocks().contains(&superblock) {
            return None;
        }
        self.superblocks
            .read()
            .get(&superblock)
            .map(|superblock| superblock.meta.cursors.blockstore.load(Acquire))
    }

    /// Iterates retained superblocks after `superblock` through the active head,
    /// so replay excludes the sealed snapshot state but includes the unsealed head.
    fn iter_after(&self, superblock: u64) -> impl Iterator<Item = Arc<Superblock>> + '_ {
        let head = self.meta.head();
        superblock
            .checked_add(1)
            .into_iter()
            .flat_map(move |start| start..=head)
            .filter_map(|id| self.superblocks.read().get(&id).cloned())
    }

    /// Opens ledger metadata and retained superblocks without starting services.
    fn new(directory: PathBuf, size_limit: u64) -> Result<Self> {
        fs::create_dir_all(&directory)?;
        let index = Index::new(&directory)?;
        let meta = directory.join(LEDGER_META);
        // SAFETY: `LedgerMeta` and its nested headers have stable C layouts,
        // and all fields that can change while mapped are atomic. This process
        // exclusively creates and updates the metadata file at `meta`.
        let meta = unsafe { MetaMap::<LedgerMeta>::new(&meta) }?;
        let retained = meta.superblocks();
        let mut superblocks = BTreeMap::new();
        for id in retained {
            let superblock = Superblock::open(&directory, id, &index)?;
            superblocks.insert(id, superblock);
        }

        info!(?directory, superblocks = superblocks.len(), "opened ledger");
        Ok(Self {
            meta,
            superblocks: superblocks.into(),
            index,
            directory,
            size_limit,
        })
    }

    /// Returns true when the ledger filesystem has reached the configured limit.
    ///
    /// This assumes `directory` is on a filesystem dedicated to the ledger. Any
    /// unrelated files on the same filesystem count toward the used byte total.
    fn size_exceeded(&self) -> Result<bool> {
        let st = rustix::fs::statvfs(&self.directory)?;
        let total = st.f_blocks * st.f_frsize;
        let free = st.f_bfree * st.f_frsize;
        Ok(total.saturating_sub(free) >= self.size_limit)
    }

    /// Removes the oldest sealed superblock while keeping the active head.
    ///
    /// Superblock slot ranges are sequential and non-overlapping, so the next
    /// retained start slot is the removed superblock end plus one. Metadata is
    /// flushed before returning the cleanup worker, which removes the keyspace
    /// and directory. The caller must join the worker to observe cleanup errors.
    pub fn truncate(&self) -> Result<Option<JoinHandle<Result<()>>>> {
        let mut superblocks = self.superblocks.write();
        let Some((&id, _)) = superblocks.first_key_value() else {
            return Ok(None);
        };
        if id >= self.meta.head() {
            return Ok(None);
        }
        let Some((_, superblock)) = superblocks.pop_first() else {
            return Ok(None);
        };
        drop(superblocks);

        let timer = metrics::time(metrics::Operation::Truncate);
        let end = superblock.meta.range.end.load(Acquire);
        self.meta.superblocks.fetch_sub(1, Release);
        self.meta.range.start.store(end + 1, Release);
        self.meta.flush()?;

        let index = self.index.clone();
        Ok(Some(thread::spawn(move || {
            index.delete(superblock.index.clone())?;
            superblock.purge()?;
            info!(id, end, "purged oldest superblock");
            drop(timer);
            Ok(())
        })))
    }
}

/// Cloneable senders over shared ledger state: append events, read requests, and
/// blockstore-position updates.
#[derive(Deref, Clone)]
pub struct LedgerHandle {
    /// Shared ledger state behind the request senders.
    #[deref]
    ledger: Arc<Ledger>,
    /// Read request queue consumed by reader workers.
    pub reader: ReaderSender,
    /// Append event queue consumed by the appender worker.
    pub appender: flume::Sender<Event>,
    /// Blockstore position broadcast after committed blocks and rotations, so
    /// replication can stream bytes and follow successor boundaries.
    pub position: broadcast::Sender<BlockstorePosition>,
}

impl LedgerHandle {
    /// Returns the number of transactions published at complete block boundaries.
    pub fn transactions(&self) -> u64 {
        self.ledger.meta.transactions.load(Acquire)
    }

    /// Returns the active superblock id.
    pub fn head(&self) -> u64 {
        self.ledger.meta.head()
    }

    /// Position of the next byte to append: active superblock plus its published cursor.
    pub fn position(&self) -> BlockstorePosition {
        let superblock = self.head();
        let offset = self.cursor(superblock).unwrap_or_default();
        BlockstorePosition { superblock, offset }
    }
}

/// Opened superblock files kept alive for active and retained readers.
pub struct Superblock {
    /// Superblock id, matching its directory suffix under the ledger root.
    pub id: u64,
    /// Mmap-backed metadata for file cursors and slot range.
    meta: MetaMap<SuperblockMeta>,
    /// Transaction stream delimited by block entries and a superblock seal.
    pub blockstore: File,
    /// Transaction execution metadata file.
    executions: File,
    /// Keyspace containing this superblock's namespaced index entries.
    index: Keyspace,
    /// Superblock directory path.
    pub directory: PathBuf,
}

impl Superblock {
    /// Canonical directory and keyspace name for one superblock.
    fn name(id: u64) -> String {
        format!("superblock-{id:0>9}")
    }

    /// Returns the directory path for `id` under `root`.
    pub fn init_dir(root: &Path, id: u64) -> Result<PathBuf> {
        let dir = root.join(Self::name(id));
        fs::create_dir_all(&dir).map_err(Into::into).map(|()| dir)
    }

    /// Accountsdb snapshot checksum recorded by the seal that opened this superblock.
    pub fn checksum(&self) -> u64 {
        self.meta.checksum.load(Acquire)
    }

    /// Transaction count recorded by the seal that opened this superblock.
    pub fn transactions(&self) -> u64 {
        self.meta.transactions.load(Acquire)
    }

    /// Opens a superblock directory, creating its data files when needed.
    fn open(root: &Path, id: u64, index: &Index) -> Result<Arc<Self>> {
        let directory = Self::init_dir(root, id)?;
        let meta = unsafe { MetaMap::<SuperblockMeta>::new(&directory.join(SUPERBLOCK_META)) }?;
        if meta.version != VERSION {
            return Err(LedgerError::UnsupportedVersion(meta.version));
        }
        let index = index.keyspace(id)?;
        let blockstore = Self::file(&directory.join(BLOCKSTORE_DB))?;
        let executions = Self::file(&directory.join(EXECUTIONS_DB))?;

        Ok(Arc::new(Self {
            id,
            meta,
            blockstore,
            executions,
            index,
            directory,
        }))
    }

    /// Opens a superblock data file for random reads.
    fn file(path: &Path) -> Result<File> {
        drop(File::options().write(true).create(true).truncate(false).open(path)?);
        let f = File::open(path)?;
        #[cfg(target_os = "linux")]
        // Access advice is an optional optimization and must not prevent opening the ledger.
        let _ = rustix::fs::fadvise(&f, 0, None, rustix::fs::Advice::Random);

        Ok(f)
    }

    /// Removes this superblock directory from the ledger root.
    fn purge(&self) -> Result<()> {
        fs::remove_dir_all(&self.directory).map_err(Into::into)
    }
}
