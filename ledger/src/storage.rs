//! Superblock storage primitives.
//!
//! This module keeps the low-level storage surface in one place: buffered
//! append files and mmap-backed metadata headers.

use std::{
    fs::File,
    io::{self, Cursor, Write},
    ops::{Deref, Range},
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::FileExt,
    },
    path::Path,
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering::*},
};

use memmap2::{MmapMut, MmapOptions};
use nucleus::{MB, Slot};
use rustix::fs::{self, FallocateFlags};
use tracing::debug;
use zstd::bulk::Compressor;

use crate::{Result, index::Span, schema::MAX_ENTRY_SIZE};

/// Initial buffer size for append-heavy files.
const FILE_BUFFER_SIZE: usize = 64 * MB;
/// Physical space reserved when an append file is close to its allocated end.
// NOTE: fallocate is slow on non-linux, so we use tiny increments for tests/dev
#[cfg(any(test, target_os = "macos"))]
const PREALLOCATION_SIZE: u64 = MB as u64;
#[cfg(not(any(test, target_os = "macos")))]
const PREALLOCATION_SIZE: u64 = 4 * nucleus::GB as u64;

/// Remaining allocation that triggers another reservation.
const PREALLOCATION_THRESHOLD: u64 = PREALLOCATION_SIZE / 16;

/// Buffered append writer that tracks logical file position.
pub(crate) struct AppendFile {
    /// Backing file.
    pub(crate) file: File,
    /// Pending bytes not yet flushed to the backing file.
    buffer: Vec<u8>,
    /// Logical byte position including buffered bytes.
    pub(crate) cursor: u64,
    len: u64,
}

impl AppendFile {
    /// Opens an append file with spare capacity past its flush threshold.
    pub(crate) fn new(path: &Path, cursor: &AtomicU64) -> Result<Self> {
        let file =
            File::options().create(true).truncate(false).read(true).write(true).open(path)?;
        let cursor = cursor.load(Acquire);
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            buffer: Vec::with_capacity(FILE_BUFFER_SIZE + MAX_ENTRY_SIZE),
            cursor,
            len,
        })
    }

    /// Compresses directly into the pending buffer and advances the logical cursor.
    pub(crate) fn compress(
        &mut self,
        bytes: &[u8],
        compressor: &mut Compressor<'static>,
    ) -> io::Result<()> {
        let buffered = self.buffer.len();
        let written = {
            let mut destination = Cursor::new(&mut self.buffer);
            destination.set_position(buffered as u64);
            compressor.compress_to_buffer(bytes, &mut destination)?
        };
        self.cursor += written as u64;
        if self.buffer.len() >= FILE_BUFFER_SIZE {
            self.flush()?;
        }
        Ok(())
    }

    /// Buffers a complete encoded fragment and advances the logical cursor.
    #[inline]
    pub(crate) fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.buffer.extend_from_slice(bytes);
        self.cursor += bytes.len() as u64;
        if self.buffer.len() >= FILE_BUFFER_SIZE {
            self.flush()?;
        }
        Ok(())
    }

    /// Flushes buffered bytes, syncs file data, and returns the durable cursor.
    pub(crate) fn sync(&mut self) -> Result<u64> {
        if self.len.saturating_sub(self.cursor) < PREALLOCATION_THRESHOLD {
            self.preallocate()?;
        }
        self.flush()?;
        self.file.sync_data()?;
        Ok(self.cursor)
    }

    /// Extends the physical file without changing the logical append cursor.
    fn preallocate(&mut self) -> Result<()> {
        let offset = self.len;
        if (offset + PREALLOCATION_SIZE) > Span::MAX_FILE_SIZE {
            Err(io::Error::from(io::ErrorKind::FileTooLarge))?;
        }
        fs::fallocate(
            self.file.as_fd(),
            FallocateFlags::empty(),
            offset,
            PREALLOCATION_SIZE,
        )?;
        self.file.sync_all()?;
        self.len = offset + PREALLOCATION_SIZE;
        debug!(len = self.len, "preallocated ledger file space");
        Ok(())
    }

    /// Trims unused preallocated space after the superblock is sealed.
    pub(crate) fn finalize(&mut self) -> Result<()> {
        self.flush()?;
        self.file.set_len(self.cursor)?;
        self.file.sync_all()?;
        self.len = self.cursor;
        Ok(())
    }
}

impl Write for AppendFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.append(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        // `cursor` includes the buffered bytes; indexes use these logical offsets.
        let offset = self.cursor - self.buffer.len() as u64;
        self.file.write_all_at(&self.buffer, offset)?;
        self.buffer.clear();
        self.len = self.len.max(self.cursor);
        Ok(())
    }
}

impl wincode::io::Writer for AppendFile {
    #[inline]
    fn write(&mut self, src: &[u8]) -> wincode::io::WriteResult<()> {
        self.append(src)?;
        Ok(())
    }
}

/// Mmap-backed fixed-size metadata header.
pub(crate) struct MetaMap<T> {
    /// Typed pointer into the mapped header.
    data: NonNull<T>,
    /// Mutable mapping kept alive for `data`.
    mmap: MmapMut,
}

impl<T: Default + Sized> MetaMap<T> {
    /// Opens `path` and initializes it with `T::default()` when empty.
    ///
    /// # Safety
    ///
    /// `T` must be a plain fixed-layout metadata header that can be safely
    /// read from bytes previously written by this process. All shared mutation
    /// inside `T` must use synchronization such as atomics.
    #[allow(unsafe_op_in_unsafe_fn)]
    pub(crate) unsafe fn new(path: &Path) -> Result<Self> {
        let size = size_of::<T>().div_ceil(512) * 512;
        let file =
            File::options().write(true).read(true).create(true).truncate(false).open(path)?;
        if file.metadata()?.len() == 0 {
            file.set_len(size as u64)?;
            let mut mmap = MmapOptions::new().len(size).map_mut(file.as_raw_fd())?;
            mmap.as_mut_ptr().cast::<T>().write(T::default());
            mmap.flush()?;
            let data = NonNull::new_unchecked(mmap.as_mut_ptr().cast::<T>());
            return Ok(Self { data, mmap });
        }

        // SAFETY: `size` matches the fixed metadata header size used when the
        // file was created, and `mmap` is owned by the returned `MetaMap`.
        let mut mmap = MmapOptions::new().len(size).map_mut(file.as_raw_fd())?;
        // SAFETY: mmap pointers are non-null for non-empty mappings.
        let data = NonNull::new_unchecked(mmap.as_mut_ptr().cast::<T>());
        Ok(Self { data, mmap })
    }

    /// Flushes metadata changes to disk.
    #[inline]
    pub(crate) fn flush(&self) -> Result<()> {
        self.mmap.flush().map_err(Into::into)
    }
}

impl<T> Deref for MetaMap<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { self.data.as_ref() }
    }
}

// SAFETY: `MetaMap` owns the mapping that backs `data`; moving the wrapper
// does not invalidate the pointer. Cross-thread access is constrained by `T`.
unsafe impl<T: Send> Send for MetaMap<T> {}

// SAFETY: shared access exposes only `&T`. The metadata headers used by the
// ledger mutate shared state through atomics.
unsafe impl<T: Sync> Sync for MetaMap<T> {}

/// Ledger-wide metadata header.
#[repr(C)]
pub(crate) struct LedgerMeta {
    /// Total transactions committed since genesis.
    pub(crate) transactions: AtomicU64,
    /// Number of retained superblocks.
    pub(crate) superblocks: AtomicU64,
    /// Active superblock identifier.
    pub(crate) head: AtomicU64,
    /// Inclusive slot range covered by retained superblocks.
    pub(crate) range: BlockRange,
    /// Total blocks committed since genesis.
    pub(crate) blocks: AtomicU64,
}

impl Default for LedgerMeta {
    fn default() -> Self {
        Self {
            transactions: 0.into(),
            head: 1.into(),
            superblocks: 1.into(),
            range: Default::default(),
            blocks: 0.into(),
        }
    }
}

impl LedgerMeta {
    /// Returns the active superblock id.
    #[inline]
    pub(crate) fn head(&self) -> u64 {
        self.head.load(Acquire)
    }

    /// Returns the retained superblock id range, including the active head.
    #[inline]
    pub(crate) fn superblocks(&self) -> Range<u64> {
        let head = self.head();
        let start = head - self.superblocks.load(Acquire).saturating_sub(1);
        start..head + 1
    }
}

/// Metadata header for one superblock directory.
#[derive(Default)]
#[repr(C)]
pub(crate) struct SuperblockMeta {
    /// Durable append cursors for files in this superblock.
    pub(crate) cursors: FileCursors,
    /// Slot range stored in this segment.
    pub(crate) range: BlockRange,
    /// Accountsdb snapshot checksum carried over from the seal that opened this superblock.
    pub(crate) checksum: AtomicU64,
}

/// Durable append cursors for superblock data files.
#[derive(Default)]
#[repr(C)]
pub(crate) struct FileCursors {
    /// Durable byte cursor in `blockstore.db`.
    pub(crate) blockstore: AtomicU64,
    /// Durable byte cursor in `executions.db`.
    pub(crate) executions: AtomicU64,
}

/// Inclusive slot range covered by a superblock.
#[derive(Default)]
#[repr(C)]
pub(crate) struct BlockRange {
    /// First slot in the segment.
    pub(crate) start: AtomicU64,
    /// Last slot in the segment.
    pub(crate) end: AtomicU64,
}

impl BlockRange {
    /// Returns true when `slot` is inside the range.
    pub(crate) fn contains(&self, slot: &Slot) -> bool {
        (self.start.load(Acquire)..=self.end.load(Acquire)).contains(slot)
    }
}
