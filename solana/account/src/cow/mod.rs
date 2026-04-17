//! Copy-on-write account data with zero-copy access to aligned external storage.
//!
//! `borrowed` defines the raw buffer layout and `owned` holds the heap-backed form.
#![allow(unsafe_op_in_unsafe_fn)]

mod borrowed;
mod owned;

pub use borrowed::BorrowedAccount;
pub use owned::{AccountBuilder, AccountMode, OwnedAccount};

use crate::{Account, ReadableAccount, WritableAccount};
use solana_clock::{Epoch, Slot};
use solana_pubkey::Pubkey;
use std::{
    cell::RefCell,
    ops::{Deref, DerefMut},
    rc::Rc,
    sync::Arc,
};

#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;

use CoWAccount::*;

/// Borrowed buffers must be aligned to this many bytes.
pub const ALIGNMENT: usize = 8;
/// Bytes in one storage unit.
pub const STORAGE_UNIT: usize = size_of::<StorageUnit>();
/// Minimum addressable unit of storage (each address is a multiple of 8)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StorageUnit(pub u64);

/// Shared account data that borrows directly from an aligned external buffer
/// until a write requires promotion to owned heap storage.
///
/// Higher layers use `mutable()` to decide whether the account is routed to
/// the persisted or volatile store.
#[cfg_attr(feature = "serde", derive(serde::Deserialize), serde(from = "Account"))]
#[derive(PartialEq, Eq, Clone, Default)]
pub struct AccountSharedData {
    /// Backing storage, borrowed until promotion or direct construction.
    pub(crate) cow: CoWAccount,
    /// Fields changed through the writable APIs.
    pub(crate) dirty: DirtyMarkers,
}

/// Core account state shared by the borrowed and owned representations.
#[repr(C)]
#[derive(Clone, Copy, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct AccountCore {
    /// Lamport balance.
    pub(crate) lamports: u64,
    /// Account owner.
    pub(crate) owner: Pubkey,
    /// On-chain slot, at which the account was cloned.
    pub(crate) slot: Slot,
    /// Mutually exclusive mode of existence for the account.
    ///
    /// The engine uses this to decide whether the account is mutable.
    pub(crate) mode: AccountMode,
    /// Various state modifier flags, e.g. executable, compressed
    pub(crate) flags: StateFlags,
}

impl Deref for AccountSharedData {
    type Target = AccountCore;

    fn deref(&self) -> &Self::Target {
        match &self.cow {
            Borrowed(account) => {
                // SAFETY: `BorrowedAccount` owns the invariant that `core` points at
                // a live `AccountCore` inside the borrowed buffer.
                unsafe { account.core.as_ref() }
            }
            Owned(account) => &account.core,
        }
    }
}

impl DerefMut for AccountSharedData {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match &mut self.cow {
            Borrowed(account) => {
                // SAFETY: `&mut self` guarantees unique access to the borrowed image.
                unsafe { account.core.as_mut() }
            }
            Owned(account) => &mut account.core,
        }
    }
}

impl AccountSharedData {
    /// Returns a reference to the inner copy-on-write representation.
    pub fn cow(&self) -> &CoWAccount {
        &self.cow
    }

    /// Returns an owned copy of the current account state.
    pub fn owned(&self) -> OwnedAccount {
        match self.cow() {
            Borrowed(a) => a.into(),
            Owned(a) => a.clone(),
        }
    }

    /// Returns `true` for engine-exclusive modes that may be mutated in place.
    pub fn mutable(&self) -> bool {
        use AccountMode::*;
        matches!(self.mode, Delegated | Ephemeral | Transient)
    }

    /// Returns `true` when the account is in `mode`.
    pub fn is(&self, mode: AccountMode) -> bool {
        self.mode == mode
    }

    /// Returns the account modifier flags.
    pub fn flags(&self) -> &StateFlags {
        &self.flags
    }

    /// Returns the dirty-field markers.
    pub fn markers(&self) -> &DirtyMarkers {
        &self.dirty
    }

    /// Marks the data buffer as modified.
    pub(crate) fn mark_data_dirty(&mut self) {
        self.dirty.insert(DirtyMarkers::DATA);
    }

    /// Returns `true` when the owned buffer has more than one strong reference.
    pub fn is_shared(&self) -> bool {
        self.cow.is_shared()
    }

    /// Returns `true` if any field has been modified.
    pub fn dirty(&self) -> bool {
        self.dirty.intersects(DirtyMarkers::all())
    }

    /// Returns the current data capacity.
    pub fn capacity(&self) -> usize {
        self.cow.capacity()
    }

    /// Returns a shared owned copy of the current data bytes.
    pub fn data_clone(&self) -> Arc<Vec<u8>> {
        self.cow.data_clone()
    }

    /// Resizes the account data.
    pub fn resize(&mut self, len: usize, val: u8) {
        self.mark_data_dirty();
        self.cow.resize(len, val);
    }

    /// Appends bytes to the account data.
    pub fn extend_from_slice(&mut self, data: &[u8]) {
        self.mark_data_dirty();
        self.cow.extend_from_slice(data);
    }

    /// Replaces the account data with the provided bytes.
    pub fn set_data_from_slice(&mut self, data: &[u8]) {
        self.mark_data_dirty();
        self.cow.set_data_from_slice(data);
    }

    /// Writes bytes at `offset`, extending and zero-filling as needed.
    pub(crate) fn set_data_at(&mut self, offset: usize, data: &[u8]) {
        let len = self.data().len();
        if offset > len {
            self.resize(offset, 0);
        }

        // Write the overlap in place, then append any remaining tail. This
        // keeps borrowed buffers on the fast path when the write fits.
        let n = self.data().len().saturating_sub(offset).min(data.len());
        self.data_as_mut_slice()[offset..offset + n].copy_from_slice(&data[..n]);
        self.extend_from_slice(&data[n..]);
    }

    /// Replaces the account data with `data`.
    #[allow(unused)]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn set_data(&mut self, data: Vec<u8>) {
        self.mark_data_dirty();
        self.cow.set_data(data);
    }

    /// Sets the account's on-chain slot.
    pub(crate) fn set_slot(&mut self, slot: Slot) {
        self.dirty.set(DirtyMarkers::SLOT, true);
        self.slot = slot;
    }

    /// Sets or clears a state flag and marks the flags dirty.
    pub(crate) fn set_flag(&mut self, flag: StateFlags, val: bool) {
        self.dirty.set(DirtyMarkers::FLAGS, true);
        self.flags.set(flag, val);
    }

    /// Sets the account mode and marks the mode dirty.
    pub(crate) fn set_mode(&mut self, mode: AccountMode) {
        self.dirty.set(DirtyMarkers::MODE, true);
        self.mode = mode;
    }

    /// Creates a new owned shared-data account with zero-filled data.
    pub fn new(lamports: u64, space: usize, owner: &Pubkey) -> Self {
        AccountBuilder::default()
            .lamports(lamports)
            .data(vec![0; space])
            .owner(*owner)
            .build()
            .into()
    }
    /// Creates a new shared-data account wrapped in a `RefCell`.
    pub fn new_ref(lamports: u64, space: usize, owner: &Pubkey) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self::new(lamports, space, owner)))
    }

    /// Creates a new account with serialized data.
    #[cfg(feature = "bincode")]
    pub fn new_data<T: serde::Serialize>(
        lamports: u64,
        state: &T,
        owner: &Pubkey,
    ) -> Result<Self, bincode::Error> {
        let data = bincode::serialize(state)?;
        Ok(Self::create_from_existing_shared_data(
            lamports,
            Arc::new(data),
            *owner,
            false,
            Epoch::default(),
        ))
    }

    /// Creates a new serialized account wrapped in a `RefCell`.
    #[cfg(feature = "bincode")]
    pub fn new_ref_data<T: serde::Serialize>(
        lamports: u64,
        state: &T,
        owner: &Pubkey,
    ) -> Result<RefCell<Self>, bincode::Error> {
        Self::new_data(lamports, state, owner).map(RefCell::new)
    }

    /// Creates a new fixed-size account with serialized data.
    #[cfg(feature = "bincode")]
    pub fn new_data_with_space<T: serde::Serialize>(
        lamports: u64,
        state: &T,
        space: usize,
        owner: &Pubkey,
    ) -> Result<Self, bincode::Error> {
        let mut account = Self::new(lamports, space, owner);
        crate::codec::serialize_data(&mut account, state)?;
        Ok(account)
    }

    /// Creates a new fixed-size serialized account wrapped in a `RefCell`.
    #[cfg(feature = "bincode")]
    pub fn new_ref_data_with_space<T: serde::Serialize>(
        lamports: u64,
        state: &T,
        space: usize,
        owner: &Pubkey,
    ) -> Result<RefCell<Self>, bincode::Error> {
        Self::new_data_with_space(lamports, state, space, owner).map(RefCell::new)
    }

    /// Creates a new shared-data account.
    ///
    /// `rent_epoch` is ignored because this type does not store it.
    pub fn new_rent_epoch(lamports: u64, space: usize, owner: &Pubkey, _: Epoch) -> Self {
        Self::new(lamports, space, owner)
    }

    /// Deserializes the account data as `T`.
    #[cfg(feature = "bincode")]
    pub fn deserialize_data<T: serde::de::DeserializeOwned>(&self) -> Result<T, bincode::Error> {
        crate::codec::deserialize_data(self)
    }

    /// Serializes `state` into the existing account data buffer.
    #[cfg(feature = "bincode")]
    pub fn serialize_data<T: serde::Serialize>(&mut self, state: &T) -> Result<(), bincode::Error> {
        crate::codec::serialize_data(self, state)
    }

    /// Creates an owned shared-data account from existing shared bytes.
    ///
    /// `rent_epoch` is ignored because this type does not store it.
    pub fn create_from_existing_shared_data(
        lamports: u64,
        data: Arc<Vec<u8>>,
        owner: Pubkey,
        executable: bool,
        _: Epoch,
    ) -> Self {
        AccountBuilder::default()
            .lamports(lamports)
            .data(data)
            .owner(owner)
            .executable(executable)
            .build()
            .into()
    }
}

bitflags::bitflags! {
    /// Account state modifier flags.
    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    #[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
    pub struct StateFlags: u8 {
        /// Executable account data.
        const EXECUTABLE = 1 << 0;
        /// Compressed account (using Light protocol on solana)
        const COMPRESSED = 1 << 1;
    }

    /// Bits that record which fields changed through `AccountSharedData`.
    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    pub struct DirtyMarkers: u8 {
        /// Data bytes changed.
        const DATA     = 1 << 0;
        /// Owner changed.
        const OWNER    = 1 << 1;
        /// Lamports changed.
        const LAMPORTS = 1 << 2;
        /// Slot changed.
        const SLOT     = 1 << 3;
        /// Mode changed.
        const MODE     = 1 << 4;
        /// State flags changed.
        const FLAGS    = 1 << 5;
    }
}

/// Backing storage for `AccountSharedData`.
#[derive(PartialEq, Eq, Clone)]
pub enum CoWAccount {
    /// Borrowed image, a view into static backing buffer.
    Borrowed(BorrowedAccount),
    /// Heap-owned image.
    Owned(OwnedAccount),
}

impl CoWAccount {
    /// Promotes borrowed storage to the owned form.
    pub(crate) fn promote(&mut self) {
        let Self::Borrowed(account) = self else {
            return;
        };
        *self = Self::Owned(account.deref().into());
    }

    /// Returns the current data slice.
    pub(crate) fn data(&self) -> &[u8] {
        match self {
            Self::Borrowed(account) => &account.data,
            Self::Owned(account) => &account.data,
        }
    }

    /// Returns `true` when the heap buffer has multiple owners.
    pub(crate) fn is_shared(&self) -> bool {
        match self {
            Self::Borrowed(_) => false,
            Self::Owned(account) => Arc::strong_count(&account.data) > 1,
        }
    }

    /// Returns the current data capacity.
    pub(crate) fn capacity(&self) -> usize {
        match self {
            Self::Borrowed(account) => account.data.capacity(),
            Self::Owned(account) => account.data.capacity(),
        }
    }

    /// Returns a shared owned copy of the current data bytes.
    pub(crate) fn data_clone(&self) -> Arc<Vec<u8>> {
        match self {
            Self::Borrowed(account) => Arc::new(account.data.to_vec()),
            Self::Owned(account) => Arc::clone(&account.data),
        }
    }

    /// Returns mutable data, promoting borrowed storage only when needed.
    pub(crate) fn data_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Borrowed(account) => &mut account.data,
            Self::Owned(account) => Arc::<Vec<u8>>::make_mut(&mut account.data).as_mut_slice(),
        }
    }

    /// Reserves additional space for the account data.
    pub(crate) fn reserve(&mut self, additional: usize) {
        if let Self::Borrowed(a) = self
            && a.data.spare() >= additional
        {
            return;
        }
        self.promote();
        if let Self::Owned(account) = self {
            Arc::make_mut(&mut account.data).reserve(additional);
        }
    }

    /// Resizes the account data.
    pub(crate) fn resize(&mut self, len: usize, val: u8) {
        if let Self::Borrowed(a) = self
            && len <= a.data.capacity()
        {
            // SAFETY: this stays in the borrowed image only while the resized
            // range fits within the borrowed capacity.
            unsafe { a.data.resize(len, val) };
            return;
        }

        self.promote();
        if let Self::Owned(account) = self {
            Arc::make_mut(&mut account.data).resize(len, val);
        }
    }

    /// Appends bytes to the account data.
    pub(crate) fn extend_from_slice(&mut self, data: &[u8]) {
        self.reserve(data.len());

        match self {
            Self::Borrowed(account) => {
                // SAFETY: `reserve` keeps the borrowed image only when the appended
                // bytes fit in the remaining borrowed capacity.
                unsafe { account.data.extend(data) };
            }
            Self::Owned(account) => Arc::make_mut(&mut account.data).extend_from_slice(data),
        }
    }

    /// Replaces the account data with the provided bytes.
    pub(crate) fn set_data_from_slice(&mut self, data: &[u8]) {
        let additional = data.len().saturating_sub(self.data().len());
        self.reserve(additional);

        match self {
            Self::Borrowed(account) => {
                // SAFETY: `reserve` keeps the borrowed image only when the
                // replacement bytes fit in the borrowed capacity.
                unsafe { account.data.set(data) };
            }
            Self::Owned(account) => {
                let data_buf = Arc::make_mut(&mut account.data);
                data_buf.clear();
                data_buf.extend_from_slice(data);
            }
        }
    }

    /// Replaces the account data with the provided owned bytes.
    pub(crate) fn set_data(&mut self, data: Vec<u8>) {
        if matches!(self, Self::Borrowed(_)) {
            self.set_data_from_slice(&data);
            return;
        }

        if let Self::Owned(account) = self {
            account.data = data.into();
        }
    }
}

impl Default for CoWAccount {
    fn default() -> Self {
        Self::Owned(OwnedAccount::default())
    }
}

/// Wraps an owned account in `AccountSharedData`.
impl From<OwnedAccount> for AccountSharedData {
    fn from(value: OwnedAccount) -> Self {
        Self {
            cow: Owned(value),
            dirty: DirtyMarkers::default(),
        }
    }
}

/// Wraps a borrowed account in `AccountSharedData`.
impl From<BorrowedAccount> for AccountSharedData {
    fn from(value: BorrowedAccount) -> Self {
        Self {
            cow: Borrowed(value),
            dirty: DirtyMarkers::default(),
        }
    }
}

/// Converts a plain `Account` into shared data.
impl From<Account> for AccountSharedData {
    fn from(value: Account) -> Self {
        AccountBuilder::default()
            .lamports(value.lamports)
            .data(value.data)
            .owner(value.owner)
            .executable(value.executable)
            .build()
            .into()
    }
}
