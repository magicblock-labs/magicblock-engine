#![doc = include_str!("../README.md")]

use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering::*},
    thread,
};

pub use crate::error::{LedgerError, LedgerRequestError};
use derive_more::Deref;
use nucleus::{
    Slot,
    ledger::BlockstorePosition,
    shutdown::{Service, ShutdownManager},
};
use parking_lot::RwLock;
use tokio::sync::broadcast;
use tracing::info;

mod appender;
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
const SERVICE_QUEUE_CAPACITY: usize = 128;

/// Top-level ledger handle.
///
/// `head` in ledger metadata is the active superblock id. Older superblocks
/// stay open in `superblocks` until retention removes their directories.
pub struct Ledger {
    /// Mmap-backed ledger metadata shared with the appender.
    meta: MetaMap<LedgerMeta>,
    /// Retained superblocks keyed by id.
    superblocks: RwLock<BTreeMap<u64, Arc<Superblock>>>,
    /// Root ledger directory.
    pub directory: PathBuf,
    /// Maximum used bytes allowed on the ledger filesystem before retention runs.
    size_limit: u64,
}

impl Ledger {
    /// Opens the ledger and starts one appender plus the reader worker pool.
    pub fn init(
        directory: impl AsRef<Path>,
        size_limit: u64,
        shutdown: &mut ShutdownManager,
    ) -> Result<LedgerHandle> {
        let directory = directory.as_ref().to_owned();
        let ledger = Arc::new(Self::new(directory, size_limit)?);
        metrics::init(&ledger);
        let (appender_tx, rx) = flume::bounded(SERVICE_QUEUE_CAPACITY);
        let (position, _) = broadcast::channel(256);
        let appender = LedgerAppender::new(ledger.clone(), rx, position.clone())?;
        let sh = shutdown.handle(Service::LedgerAppender);
        thread::Builder::new()
            .name("ledger-appender".into())
            .spawn(|| appender.run(sh))?;
        let (reader_tx, rx) = flume::bounded(SERVICE_QUEUE_CAPACITY);

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
    pub fn iter(&self) -> impl Iterator<Item = Arc<Superblock>> {
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
        self.iter().find_map(|sb| {
            (sb.id == superblock).then_some(sb.meta.cursors.blockstore.load(Acquire))
        })
    }

    /// Iterates retained superblocks after `superblock` through the active head,
    /// so replay excludes the sealed snapshot state but includes the unsealed head.
    fn iter_after(&self, superblock: u64) -> impl Iterator<Item = Arc<Superblock>> {
        let range = superblock + 1..=self.meta.head();
        range.filter_map(|id| self.superblocks.read().get(&id).cloned())
    }

    /// Opens ledger metadata and retained superblocks without starting services.
    fn new(directory: PathBuf, size_limit: u64) -> Result<Self> {
        fs::create_dir_all(&directory)?;
        let meta = directory.join(LEDGER_META);
        // SAFETY: `LedgerMeta` and its nested headers have stable C layouts,
        // and all fields that can change while mapped are atomic. This process
        // exclusively creates and updates the metadata file at `meta`.
        let meta = unsafe { MetaMap::<LedgerMeta>::new(&meta) }?;
        let mut superblocks = BTreeMap::new();
        for id in meta.superblocks() {
            let superblock = Superblock::open(&directory, id)?;
            superblocks.insert(id, superblock);
        }

        info!(?directory, superblocks = superblocks.len(), "opened ledger");
        Ok(Self {
            meta,
            superblocks: superblocks.into(),
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
    /// retained start slot is the removed superblock end plus one.
    ///
    /// The appender runs this at a block boundary once the ledger filesystem is
    /// over its size limit; calling it directly forces a single retention pass
    /// regardless of that limit.
    pub fn truncate(&self) -> Result<()> {
        let _timer = metrics::time(metrics::Operation::Truncate);
        let Some((id, superblock)) = self
            .superblocks
            .read()
            .first_key_value()
            .map(|(id, superblock)| (*id, superblock.clone()))
        else {
            return Ok(());
        };
        if id >= self.meta.head() {
            return Ok(());
        }

        let end = superblock.meta.range.end.load(Acquire);
        superblock.purge()?;
        self.superblocks.write().remove(&id);
        self.meta.superblocks.fetch_sub(1, Release);
        self.meta.range.start.store(end + 1, Release);
        self.meta.flush()?;
        info!(id, end, "purged oldest superblock for retention");
        Ok(())
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
    /// Blockstore write position broadcast after every committed block, so the
    /// replication path can stream newly appended bytes.
    pub position: broadcast::Sender<BlockstorePosition>,
}

impl LedgerHandle {
    /// Returns the number of transactions published at durable sync boundaries.
    pub fn transactions(&self) -> u64 {
        self.ledger.meta.transactions.load(Acquire)
    }

    /// Returns the active superblock id.
    pub fn head(&self) -> u64 {
        self.ledger.meta.head()
    }

    /// Position of the next byte to append: active superblock plus its durable cursor.
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
    /// LMDB index for this superblock.
    index: Arc<Index>,
    /// Superblock directory path.
    pub directory: PathBuf,
}

impl Superblock {
    /// Returns the directory path for `id` under `root`.
    pub fn init_dir(root: &Path, id: u64) -> Result<PathBuf> {
        let dir = root.join(format!("superblock-{id:0>9}"));
        fs::create_dir_all(&dir).map_err(Into::into).map(|()| dir)
    }

    /// Accountsdb snapshot checksum recorded by the seal that opened this superblock.
    pub fn checksum(&self) -> u64 {
        self.meta.checksum.load(Acquire)
    }

    /// Opens a superblock directory, creating its data files when needed.
    fn open(root: &Path, id: u64) -> Result<Arc<Self>> {
        let directory = Self::init_dir(root, id)?;
        let index = Arc::new(Index::new(&directory)?);
        let meta = unsafe { MetaMap::<SuperblockMeta>::new(&directory.join(SUPERBLOCK_META)) }?;
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
