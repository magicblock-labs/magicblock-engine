//! Mapped storage for persisted account images.
//!
//! The file reserves a small meta header at the front, followed by the raw
//! account images written in `solana-account`'s borrowed layout.

use std::{
    fs::File,
    io::{self, Write},
    ops::Range,
    os::fd::AsRawFd,
    path::Path,
    ptr::NonNull,
    sync::atomic::{AtomicU32, AtomicU64, Ordering::*},
};

use memmap2::{MmapMut, MmapOptions};
use parking_lot::Mutex;
use solana_account::{STORAGE_UNIT, StorageUnit};

use crate::{
    AccountsDBError, Result,
    store::{DatabaseVersion, VERSION, kv::Offset},
};

/// Bytes reserved at the front of the mapped file for metadata.
const DATABASE_META_RESERVATION: usize = 256;
/// Filename used for the mapped storage file.
const STORAGE_FILE: &str = "storage.db";
/// LMDB map size for the index database.
pub(crate) const INDEX_MAP_SIZE: usize = 1024 * 1024 * 1024;
/// Growth block for the mapped storage file.
pub(crate) const STORAGE_BLOCK: u64 = 256 * 1024 * 1024;
/// Initial file size: one storage block plus the metadata reservation.
const INIT_STORAGE_SIZE: u64 = STORAGE_BLOCK + DATABASE_META_RESERVATION as u64;
/// Maximum mapped storage size.
const MMAP_SIZE: usize = u32::MAX as usize * STORAGE_UNIT + DATABASE_META_RESERVATION;

/// One allocation inside the mapped storage.
pub(crate) struct Allocation {
    /// Offset from the start of the storage area, in storage units.
    pub(crate) offset: Offset,
    /// Pointer to the start of the allocated image.
    pub(crate) ptr: NonNull<StorageUnit>,
}

/// Mapped storage backing persisted account images.
pub(crate) struct MappedStorage {
    /// Pointer to the reserved metadata header.
    meta: NonNull<DatabaseMeta>,
    /// Full file mapping.
    mmap: MmapMut,
    /// Start of the account image region.
    head: NonNull<StorageUnit>,
    /// File handle used for resizing.
    file: Mutex<File>,
}

#[repr(C)]
#[derive(Default)]
/// Runtime counters for the persisted backend.
pub(crate) struct Stats {
    /// Persisted image loads.
    reads: AtomicU64,
    /// `BorrowedAccount::commit` calls.
    commits: AtomicU64,
    /// Fresh allocations on backing storage.
    allocs: AtomicU64,
    /// Freelist allocation reuse.
    reallocs: AtomicU64,
    /// Account relocations during defrag.
    compactions: AtomicU64,
    /// Persisted deletes.
    removals: AtomicU64,
    /// File resizes.
    resizes: AtomicU64,
}

impl Stats {
    /// Counts one persisted read.
    pub(crate) fn read(&self) {
        self.reads.fetch_add(1, Relaxed);
    }

    /// Counts one borrowed account commit.
    pub(crate) fn commit(&self) {
        self.commits.fetch_add(1, Relaxed);
    }

    /// Counts one fresh allocation from the mapped file.
    pub(crate) fn alloc(&self) {
        self.allocs.fetch_add(1, Relaxed);
    }

    /// Counts one freelist reuse.
    pub(crate) fn reallocs(&self) {
        self.reallocs.fetch_add(1, Relaxed);
    }

    /// Counts one relocation during defragmentation.
    pub(crate) fn compactions(&self) {
        self.compactions.fetch_add(1, Relaxed);
    }

    /// Counts one persisted removal.
    pub(crate) fn remove(&self) {
        self.removals.fetch_add(1, Relaxed);
    }

    /// Counts one file resize.
    pub(crate) fn resize(&self) {
        self.resizes.fetch_add(1, Relaxed);
    }
}

#[repr(C)]
#[derive(Default)]
/// Metadata header stored at the front of the mapped file.
pub(crate) struct DatabaseMeta {
    /// On-disk format version.
    version: DatabaseVersion,
    /// Last computed database checksum.
    checksum: AtomicU64,
    /// Current slot.
    pub(crate) slot: AtomicU64,
    /// Current backing file length in bytes.
    len: AtomicU64,
    /// Database statistics
    stats: Stats,
    /// Next allocation cursor.
    pub(super) cursor: AtomicU32,
}

impl MappedStorage {
    /// Opens or creates the mapped storage file.
    pub(crate) fn new(path: &Path) -> Result<Self> {
        let path = path.join(STORAGE_FILE);
        let mut file = File::options().create(true).append(true).read(true).open(path)?;
        let fd = file.as_raw_fd();
        // SAFETY: the file is opened read/write and mapped for the full fixed size.
        let mut mmap = unsafe { MmapOptions::new().len(MMAP_SIZE).map_mut(fd)? };
        if file.metadata()?.len() == 0 {
            file.set_len(INIT_STORAGE_SIZE)?;
            file.flush()?;
            let meta = DatabaseMeta {
                version: VERSION,
                len: INIT_STORAGE_SIZE.into(),
                ..Default::default()
            };
            // SAFETY: the first bytes of the mapping are reserved for `DatabaseMeta`.
            unsafe { mmap.as_mut_ptr().cast::<DatabaseMeta>().write(meta) };
            mmap.flush()?;
        }
        // SAFETY: the mapping is at least `DATABASE_META_RESERVATION` bytes long,
        // so the meta header and account head pointers stay within the map.
        let (meta, head) = unsafe {
            let head = mmap.as_mut_ptr().add(DATABASE_META_RESERVATION);
            let head = NonNull::new_unchecked(head.cast());
            let meta = NonNull::new_unchecked(mmap.as_mut_ptr().cast());
            (meta, head)
        };
        let file = Mutex::new(file);
        Ok(Self { meta, mmap, head, file })
    }

    /// Flushes dirty pages to durable storage and publishes the checksum on sync flushes.
    pub(crate) fn flush(&self, sync: bool) -> io::Result<()> {
        let range = self.active();
        if sync {
            // Publish the checksum only when the caller wants a synchronous flush.
            self.meta().checksum.store(self.checksum(), Release);
            self.mmap.flush_range(range.start, range.end)
        } else {
            self.mmap.flush_async_range(range.start, range.end)
        }
    }

    /// Validates the opened storage for consistency
    pub(crate) fn validate(&self) -> Result<()> {
        if self.cursor() == 0 {
            return Ok(());
        }
        let meta = self.meta();
        if meta.version != VERSION {
            return Err(AccountsDBError::UnsupportedVersion(meta.version));
        }
        if meta.checksum.load(Acquire) != self.checksum() {
            return Err(AccountsDBError::Corruption);
        }

        Ok(())
    }

    /// Returns a pointer inside the account image region.
    pub(crate) fn at(&self, offset: Offset) -> NonNull<StorageUnit> {
        // SAFETY: `offset` is derived from the index and stays within the mapped region.
        unsafe { self.head.add(offset.0 as usize) }
    }

    /// Returns the runtime counters.
    pub(crate) fn stats(&self) -> &Stats {
        &self.meta().stats
    }

    /// Allocates a fresh span of `units` storage units.
    pub(crate) fn allocate(&self, units: u32) -> Result<Allocation> {
        let meta = self.meta();
        let offset = meta.cursor.fetch_add(units, Release);
        let len = meta.len.load(Acquire);
        // Compare the reserved span in storage units against the file length in bytes.
        let needed = Self::bytes((offset + units) as u64);
        if needed > len {
            // Grow in fixed blocks so future allocations stay amortized.
            self.grow(needed)?;
        }
        self.stats().alloc();
        let offset = Offset(offset);
        let ptr = self.at(offset);
        Ok(Allocation { offset, ptr })
    }

    /// Returns a mutable reference to the metadata header.
    pub(crate) fn meta(&self) -> &mut DatabaseMeta {
        // SAFETY: `meta` points to the reserved header at the front of the map.
        unsafe { &mut *self.meta.as_ptr() }
    }

    /// Returns the current allocation cursor in storage units.
    pub(crate) fn cursor(&self) -> u32 {
        self.meta().cursor.load(Acquire)
    }

    /// Shrinks the file to the current cursor.
    pub(super) fn shrink(&self, units: u32) -> io::Result<()> {
        self.resize(Self::bytes(units as u64), u64::le)?;
        self.meta().cursor.store(units, Release);
        Ok(())
    }

    /// Returns the active byte range, including the metadata reservation.
    fn active(&self) -> Range<usize> {
        0..Self::bytes(self.cursor() as u64) as usize
    }

    /// Converts storage units into file bytes, including the metadata reservation.
    fn bytes(units: u64) -> u64 {
        units * STORAGE_UNIT as u64 + DATABASE_META_RESERVATION as u64
    }

    /// Computes the checksum of the active segment of the mapped file.
    fn checksum(&self) -> u64 {
        use std::hash::Hasher;
        let mut hasher = twox_hash::XxHash3_64::new();
        hasher.write(&self.mmap[self.active()]);
        hasher.finish()
    }

    /// Grows the file to at least `len` bytes.
    /// Rounds up to a storage block before resizing.
    fn grow(&self, mut len: u64) -> Result<()> {
        len = len.div_ceil(STORAGE_BLOCK) * STORAGE_BLOCK;
        if len > MMAP_SIZE as u64 {
            return Err(AccountsDBError::Allocation);
        }
        self.resize(len, u64::ge).map_err(Into::into)
    }

    /// Resizes the file when the current size does not satisfy `cmp`.
    ///
    /// The file is updated before the new size is published into metadata so
    /// readers never observe a larger size than the actual mapping.
    fn resize(&self, len: u64, cmp: fn(&u64, &u64) -> bool) -> io::Result<()> {
        let mut file = self.file.lock();
        if cmp(&file.metadata()?.len(), &len) {
            return Ok(());
        }
        // Resize the file first, then publish the new size into metadata.
        file.set_len(len)?;
        file.flush()?;
        self.meta().len.store(len, Release);
        self.stats().resize();
        self.mmap.flush()?;
        Ok(())
    }
}
