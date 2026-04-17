//! Raw layout used by the borrowed zero-copy account view.
//!
//! The buffer is 8-byte aligned and contains a header followed by two images.
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

/// Header that prefixes a double-allocation borrowed account buffer.
#[repr(C, align(8))]
pub(crate) struct AccountHeader {
    /// Sequence counter; parity selects the active image.
    pub(crate) sequence: AtomicU32,
    /// Image size measured in `AccountHeader` units.
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
#[derive(Clone, Eq, PartialEq)]
pub struct BorrowedAccount {
    /// Header pointer for the borrowed buffer.
    pub(crate) header: NonNull<AccountHeader>,
    /// Pointer to the active image's account core.
    pub(crate) core: NonNull<AccountCore>,
    /// Borrowed data bytes for the active image.
    pub(crate) data: DataSlice,
}

/// Returns the byte offset for the active or shadow image.
#[inline]
fn offset(header: &AccountHeader, active: bool) -> usize {
    // Even sequence => image A is active, odd sequence => image B is active.
    let even = header.sequence.load(Acquire).is_multiple_of(2);
    // Flip to the shadow image when `active` does not match the current parity.
    let step = (active ^ even) as u32;
    (step * header.space) as usize + IMAGE_OFFSET
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

    /// Reads the account's pubkey stored in the image prefix.
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
    /// buffer whose first bytes are the account header, followed by two
    /// image-sized payloads. The active image is selected from the header
    /// sequence parity.
    pub unsafe fn init(buffer: NonNull<StorageUnit>) -> Self {
        let header = buffer.cast();
        let offset = offset(header.as_ref(), true);

        let core = header.add(offset).cast();
        let data = DataSlice::init(core.add(1).cast());

        Self { header, core, data }
    }

    /// Copies the active image into the shadow image and switches to it.
    ///
    /// # Safety
    ///
    /// The borrowed image must still be the one selected by `init`.
    pub unsafe fn translate(&mut self) {
        let offset = offset(self.header.as_ref(), false);

        // Copy bytes in bulk from active image to the shadow
        let dst = self.header.add(offset).cast();
        let src = self.core.cast::<StorageUnit>();
        let count = self.header.as_ref().space as usize;
        dst.copy_from_nonoverlapping(src, count);
        // Switch the pointers to the shadow view
        self.core = dst.cast();
        self.data = DataSlice::init(self.core.add(1).cast());
    }

    /// Commits the shadow image by advancing the sequence counter.
    pub fn commit(&self) {
        // SAFETY: the header is part of the borrowed buffer for the lifetime of `self`.
        unsafe { self.header.as_ref().sequence.fetch_add(1, Release) };
    }

    /// Repoints this view to the currently active image without copying data.
    ///
    /// # Safety
    ///
    /// The header must remain live, and `self` must be a view previously produced
    /// by [`Self::init`] or [`Self::translate`] for that borrowed buffer.
    pub unsafe fn reset(&mut self) {
        let offset = offset(self.header.as_ref(), true);
        self.core = self.header.add(offset).cast();
        self.data = DataSlice::init(self.core.add(1).cast());
    }

    /// Undoes the latest commit, by adjusting the sequence counter
    ///
    /// # Safety
    ///
    /// Call this only after `commit` to avoid data corruption;
    pub unsafe fn rollback(&self) {
        // SAFETY: the header is part of the borrowed buffer for the lifetime of `self`.
        unsafe { self.header.as_ref().sequence.fetch_sub(1, Release) };
    }

    /// Returns the owner pubkey from the active image.
    pub fn owner(&self) -> Pubkey {
        // SAFETY: `core` points at a live `AccountCore` inside the borrowed buffer.
        unsafe { self.core.as_ref() }.owner
    }

    /// Returns the serialized active image bytes that define account state.
    ///
    /// The slice starts at `AccountCore`, includes the `DataHeader`, and stops
    /// after initialized data. It excludes the shared header, pubkey prefix,
    /// inactive shadow image, and spare data capacity.
    pub fn storage(&self) -> &[u8] {
        let len = STATIC_SIZE + self.data.len();
        // SAFETY: `core` points at the active image and `len` only covers its
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
        debug_assert!(len <= cap);
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
        header.len as usize
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
        debug_assert!(data.len() <= self.capacity().saturating_sub(len));
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
        debug_assert!(data.len() <= self.capacity());
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
