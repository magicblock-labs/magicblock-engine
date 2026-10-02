//! Raw layout used by the borrowed zero-copy account view.
//!
//! The 8-byte-aligned buffer contains a header, a shared pubkey, and two images.
//! `AccountHeader::sequence` selects the active image; `translate` copies it to the shadow
//! image, `reset` repoints the view to the active image, `commit` publishes the shadow image,
//! and `rollback` undoes that publication by decrementing the sequence counter.

#![allow(unsafe_op_in_unsafe_fn)]

use std::{
    ops::{Deref, DerefMut},
    ptr::NonNull,
    slice,
    sync::atomic::{AtomicU32, Ordering::*},
};

use solana_pubkey::Pubkey;

use super::owned::OwnedAccount;
use super::{ALIGNMENT, AccountCore, STORAGE_UNIT, StorageUnit};

/// Fixed bytes in one image after the shared pubkey prefix: core and data header.
pub(super) const STATIC_SIZE: usize = size_of::<AccountCore>() + size_of::<DataHeader>();
/// Storage-unit offset from the header to the first image payload, including the pubkey prefix.
pub(super) const IMAGE_OFFSET: usize =
    (size_of::<AccountHeader>() + size_of::<Pubkey>()) / STORAGE_UNIT;

/// Header selecting the active image in a two-image borrowed account buffer.
#[repr(C, align(8))]
pub(crate) struct AccountHeader {
    /// Sequence counter; parity selects the active image.
    pub(crate) sequence: AtomicU32,
    /// Size of each image in 8-byte storage units, excluding the shared prefix.
    pub(crate) space: u32,
}

impl AccountHeader {
    /// Creates a header for one image size in storage units.
    pub(crate) fn new(space: u32) -> Self {
        // `space` stays in storage units so the active
        // image can be indexed with one multiply.
        Self { sequence: 0.into(), space }
    }
}

/// Pointer arithmetic relies on these size and alignment invariants.
const _: () = assert!(size_of::<AccountHeader>() == ALIGNMENT);
const _: () = assert!(size_of::<AccountHeader>() == STORAGE_UNIT);
const _: () = assert!((size_of::<Pubkey>() + STORAGE_UNIT) / ALIGNMENT == IMAGE_OFFSET);

/// Borrowed zero-copy account view into an aligned external buffer.
#[derive(Eq, PartialEq)]
pub struct BorrowedAccount {
    /// Header pointer for the borrowed buffer.
    pub(crate) header: NonNull<AccountHeader>,
    /// Account core for this view: active when initialized, shadow after translation.
    pub(crate) core: NonNull<AccountCore>,
    /// Data bytes from the same image as `core`.
    pub(crate) data: DataSlice,
    /// Sequence used to select this view's image.
    pub(crate) version: u32,
}

/// Returns the offset in storage units for the active or shadow image.
#[inline]
fn offset(space: u32, sequence: u32, active: bool) -> usize {
    // Even sequence => image A is active, odd sequence => image B is active.
    let even = sequence.is_multiple_of(2);
    // Select image B for an odd active sequence or an even shadow sequence.
    let step = (active ^ even) as u32;
    (step * space) as usize + IMAGE_OFFSET
}

impl BorrowedAccount {
    /// Returns the sequence value that selects the active image.
    pub(crate) fn sequence(&self) -> u32 {
        // SAFETY: borrowed account headers live for the account view.
        unsafe { self.header.as_ref() }.sequence.load(Acquire)
    }
    /// Returns the total borrowed span in `StorageUnit`s.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a valid borrowed buffer created by
    /// [`OwnedAccount::serialize`].
    pub unsafe fn span(ptr: NonNull<StorageUnit>) -> u32 {
        let space = ptr.cast::<AccountHeader>().as_ref().space;
        space * 2 + IMAGE_OFFSET as u32
    }

    /// Reads the shared pubkey stored between the header and the first image.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a valid borrowed buffer created by
    /// [`OwnedAccount::serialize`].
    pub unsafe fn pubkey(ptr: NonNull<StorageUnit>) -> Pubkey {
        *ptr.add(1).cast().as_ref()
    }

    /// Builds a borrowed account view from an aligned account buffer.
    ///
    /// # Safety
    ///
    /// `buffer` must be 8-byte aligned and point to a valid borrowed account
    /// buffer created by [`OwnedAccount::serialize`]: header, shared pubkey,
    /// and two image-sized payloads. Keep the buffer live and at the same address
    /// for every use of the returned view. Mutation requires exclusive writer
    /// access; reads racing publication require [`super::AccountSeqLock`].
    pub unsafe fn init(buffer: NonNull<StorageUnit>) -> Self {
        let header = buffer.cast::<AccountHeader>();
        let version = header.as_ref().sequence.load(Acquire);
        let offset = offset(header.as_ref().space, version, true);

        let core = header.add(offset).cast();
        let data = DataSlice::init(core.add(1).cast());

        Self { header, core, data, version }
    }

    /// Copies the active image into the shadow image and switches to it.
    ///
    /// # Safety
    ///
    /// This view must still point to the current active image selected by
    /// `init` or `reset`. The caller must hold exclusive writer access.
    pub unsafe fn translate(&mut self) {
        let offset = offset(self.header.as_ref().space, self.version, false);

        // Copy bytes in bulk from active image to the shadow
        let dst = self.header.add(offset).cast();
        let src = self.core.cast::<StorageUnit>();
        if src == dst {
            return;
        }
        let count = self.header.as_ref().space as usize;
        dst.copy_from_nonoverlapping(src, count);
        // Switch the pointers to the shadow view
        self.core = dst.cast();
        self.data = DataSlice::init(self.core.add(1).cast());
    }

    /// Publishes the shadow image if it was prepared against the current sequence.
    pub fn commit(&self) {
        // SAFETY: the header is part of the borrowed buffer for the lifetime of `self`.
        let header = unsafe { self.header.as_ref() };
        let shadow = unsafe {
            self.header.add(offset(header.space, self.version, false)).cast::<AccountCore>()
        };
        if self.core != shadow {
            return;
        }
        let next = self.version.wrapping_add(1);
        let _ = header.sequence.compare_exchange(self.version, next, Release, Relaxed);
    }

    /// Repoints this view to the currently active image without copying data.
    ///
    /// # Safety
    ///
    /// The header must remain live, and `self` must be a view previously produced
    /// by [`Self::init`] or [`Self::translate`] for that borrowed buffer.
    pub unsafe fn reset(&mut self) {
        self.version = self.header.as_ref().sequence.load(Acquire);
        let offset = offset(self.header.as_ref().space, self.version, true);
        self.core = self.header.add(offset).cast();
        self.data = DataSlice::init(self.core.add(1).cast());
    }

    /// Restores the previous active image by decrementing the sequence counter.
    ///
    /// # Safety
    ///
    /// Call only after this view successfully published the latest commit,
    /// with no intervening publication or write to the previous image.
    /// Exclusive writer access must be retained through rollback.
    pub unsafe fn rollback(&self) {
        // SAFETY: the header is part of the borrowed buffer for the lifetime of `self`.
        unsafe { self.header.as_ref().sequence.fetch_sub(1, Release) };
    }

    /// Returns the owner pubkey from this view's selected image.
    pub fn owner(&self) -> Pubkey {
        // SAFETY: `core` points at a live `AccountCore` inside the borrowed buffer.
        unsafe { self.core.as_ref() }.owner
    }

    /// Returns the serialized state bytes from this view's selected image.
    ///
    /// The slice starts at `AccountCore`, includes the `DataHeader`, and stops
    /// after initialized data. It excludes the shared header, pubkey prefix,
    /// inactive shadow image, and spare data capacity.
    pub fn storage(&self) -> &[u8] {
        let len = STATIC_SIZE + self.data.len();
        // SAFETY: `core` points at this view's image and `len` only covers its
        // initialized state bytes: core, data header, and initialized data.
        unsafe { slice::from_raw_parts(self.core.as_ptr().cast(), len) }
    }
}

impl From<&BorrowedAccount> for OwnedAccount {
    fn from(value: &BorrowedAccount) -> Self {
        Self {
            // SAFETY: `BorrowedAccount` guarantees `core` points at a live account
            // header inside the borrowed buffer for the lifetime of the borrow.
            core: *unsafe { value.core.as_ref() },
            data: value.data.deref().to_vec().into(),
        }
    }
}

/// Mutable byte slice backed by a borrowed account buffer.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct DataSlice {
    /// Header carrying length and capacity.
    header: NonNull<DataHeader>,
    /// Pointer to the first data byte.
    ptr: NonNull<u8>,
}

/// Data header stored immediately before the raw byte slice.
#[repr(C)]
pub(crate) struct DataHeader {
    /// Initialized data length.
    len: u32,
    /// Total writable capacity.
    cap: u32,
}

impl DataHeader {
    /// Creates a data header for one image.
    pub(crate) fn new(len: u32, allocation: u32) -> Self {
        // `cap` is the writable tail after `AccountCore` and `DataHeader`.
        let cap = (allocation as usize * STORAGE_UNIT - STATIC_SIZE) as u32;
        Self { len, cap }
    }
}

impl DataSlice {
    /// Builds a borrowed slice from a data header.
    ///
    /// # Safety
    ///
    /// `header` must point at a valid `DataHeader` followed by initialized data.
    unsafe fn init(header: NonNull<DataHeader>) -> Self {
        let ptr = header.add(1).cast();
        Self { header, ptr }
    }

    /// Returns the initialized byte length.
    pub(crate) fn len(&self) -> usize {
        // SAFETY: `header` points at the live data header for this borrowed slice.
        let header = unsafe { self.header.as_ref() };
        header.len.min(header.cap) as usize
    }

    /// Returns the total writable capacity.
    pub(crate) fn capacity(&self) -> usize {
        // SAFETY: `header` points at the live data header for this borrowed slice.
        let header = unsafe { self.header.as_ref() };
        header.cap as usize
    }

    /// Returns the remaining writable capacity.
    pub(crate) fn spare(&self) -> usize {
        self.capacity() - self.len()
    }

    /// Resizes the initialized range in place.
    ///
    /// # Safety
    ///
    /// `len` must not exceed the borrowed capacity.
    pub(crate) unsafe fn resize(&mut self, len: usize, val: u8) {
        let prev = self.len();
        debug_assert!(prev <= self.capacity());
        debug_assert!(len <= self.capacity());
        let delta = len.saturating_sub(prev);
        if delta > 0 {
            self.ptr.as_ptr().add(prev).write_bytes(val, delta);
        }
        self.header.as_mut().len = len as u32;
    }

    /// Appends bytes in place.
    ///
    /// # Safety
    ///
    /// `data` must fit in the remaining borrowed capacity and not overlap.
    pub(crate) unsafe fn extend(&mut self, data: &[u8]) {
        let len = self.len();
        let dst = self.ptr.as_ptr().add(len);
        dst.copy_from_nonoverlapping(data.as_ptr(), data.len());
        self.header.as_mut().len += data.len() as u32;
    }

    /// Replaces the initialized bytes in place.
    ///
    /// # Safety
    ///
    /// `data` must fit in the borrowed capacity and not overlap.
    pub(crate) unsafe fn set(&mut self, data: &[u8]) {
        self.ptr.as_ptr().copy_from_nonoverlapping(data.as_ptr(), data.len());
        self.header.as_mut().len = data.len() as u32;
    }
}

impl Deref for DataSlice {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: `len` bytes from `ptr` are initialized account data owned by
        // the borrowed buffer described by this `DataSlice`.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len()) }
    }
}

impl DerefMut for DataSlice {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the borrowed buffer grants unique mutable access through this borrow.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len()) }
    }
}

// SAFETY: `BorrowedAccount` points into external storage and only exposes
// shared reads unless the caller holds `&mut self`; moving the view to another
// thread does not weaken the buffer lifetime and aliasing requirements.
unsafe impl Send for BorrowedAccount {}
