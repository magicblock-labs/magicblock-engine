//! Copy-on-write account data with zero-copy access to aligned external storage.
//!
//! `borrowed` defines the raw buffer layout and `owned` holds the heap-backed form.
#![allow(unsafe_op_in_unsafe_fn)]

mod borrowed;
mod owned;

pub use borrowed::BorrowedAccount;
pub use owned::{AccountBuilder, OwnedAccount};

use crate::{Account, ReadableAccount, WritableAccount, patch::AccountPatchError};
use solana_clock::{Epoch, Slot};
use solana_pubkey::Pubkey;
use std::{
    cell::RefCell,
    ops::{Deref, DerefMut},
    rc::Rc,
    sync::Arc,
};

use CoWAccount::*;

/// Borrowed buffers must be aligned to this many bytes.
pub const ALIGNMENT: usize = 8;
/// Bytes in one storage unit.
pub const STORAGE_UNIT: usize = size_of::<StorageUnit>();
/// Minimum addressable storage unit for borrowed account images.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct StorageUnit(pub u64);

/// Shared account data that borrows directly from an aligned external buffer
/// until a write requires promotion to owned heap storage.
///
/// Higher layers use `mutable()` to decide whether the account is routed to
/// the persisted or volatile store.
#[cfg_attr(feature = "serde", derive(serde::Deserialize), serde(from = "Account"))]
#[derive(Clone, Default)]
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
    pub(crate) mode: AccountMode,
    /// Account state modifier flags.
    pub(crate) flags: StateFlags,
    /// Reserved bytes that make the serialized representation deterministic.
    _padding: [u8; 6],
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

impl PartialEq for AccountSharedData {
    fn eq(&self, other: &Self) -> bool {
        self.deref() == other.deref() && self.cow.data() == other.cow.data()
    }
}

impl Eq for AccountSharedData {}

impl AccountSharedData {
    /// Returns a reference to the inner copy-on-write representation.
    pub fn cow(&self) -> &CoWAccount {
        &self.cow
    }

    /// Returns mutable access to the inner copy-on-write representation.
    pub fn cow_mut(&mut self) -> &mut CoWAccount {
        &mut self.cow
    }

    /// Returns the account's on-chain slot.
    pub fn slot(&self) -> Slot {
        self.slot
    }

    /// Copies a clean borrowed image into the shadow buffer before mutation.
    pub fn translate(&mut self) {
        if self.dirty() {
            return;
        }
        if let Borrowed(ref mut acc) = self.cow {
            // SAFETY: this runs before the first dirty marker, so the borrowed
            // view still points at the active image selected by `init`.
            unsafe { acc.translate() };
        }
    }

    /// Returns an owned copy of the current account state.
    pub fn owned(&self) -> OwnedAccount {
        match self.cow() {
            Borrowed(a) => a.into(),
            Owned(a) => a.clone(),
        }
    }

    /// Returns `true` for engine-exclusive modes that may be mutated
    pub fn mutable(&self) -> bool {
        self.mode.mutable()
    }

    /// Returns the account's exact lifecycle mode.
    pub fn mode(&self) -> AccountMode {
        self.mode
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
        self.translate();
        self.mark_data_dirty();
        self.cow.resize(len, val);
    }

    /// Appends bytes to the account data.
    pub fn extend_from_slice(&mut self, data: &[u8]) {
        self.translate();
        self.mark_data_dirty();
        self.cow.extend_from_slice(data);
    }

    /// Replaces the account data with the provided bytes.
    pub fn set_data_from_slice(&mut self, data: &[u8]) {
        self.translate();
        self.mark_data_dirty();
        self.cow.set_data_from_slice(data);
    }

    /// Sets a legal account mode transition and marks the mode dirty.
    ///
    /// Reapplying the current mode is a no-op so callers can distinguish a
    /// genuine lifecycle transition from an unchanged account. Invalid
    /// transitions leave the account and its dirty markers unchanged.
    pub fn set_mode(&mut self, to: AccountMode) -> Result<(), AccountPatchError> {
        use AccountMode::*;
        let from = self.mode;

        if from == to {
            return Ok(());
        }
        let allowed = match (from, to) {
            (ReadOnly | Placeholder, to) => to != Transient,
            (Delegated, Transient) | (Transient, ReadOnly) | (Ephemeral, Closed) => true,
            _ => false,
        };
        if !allowed {
            Err(AccountPatchError::InvalidModeTransition { from, to })?;
        }
        self.translate();
        self.dirty.set(DirtyMarkers::MODE, true);
        self.mode = to;
        Ok(())
    }

    /// Writes bytes at `offset`, extending and zero-filling as needed.
    pub(crate) fn set_data_at(&mut self, offset: usize, data: &[u8]) {
        self.translate();
        self.mark_data_dirty();
        let len = self.data().len();
        if offset > len {
            // Grow to `offset`, zero-filling the gap; the write below then
            // appends `data` past it via `extend_from_slice`.
            self.resize(offset, 0);
        }

        // Write the overlap in place, then append any remaining tail. This
        // keeps borrowed buffers on the fast path when the write fits.
        let n = self.data().len().saturating_sub(offset).min(data.len());
        self.data_as_mut_slice()[offset..offset + n].copy_from_slice(&data[..n]);
        self.extend_from_slice(&data[n..]);
    }

    /// Sets a non-regressing account slot.
    ///
    /// Reapplying the current slot is accepted only after a genuine mode change
    /// in the same transaction. Rejected transitions leave the account and its
    /// dirty markers unchanged.
    pub(crate) fn set_slot(&mut self, to: Slot) -> Result<(), AccountPatchError> {
        let from = self.slot;
        let mode_changed = self.dirty.contains(DirtyMarkers::MODE);
        if to < from || (to == from && !mode_changed) {
            Err(AccountPatchError::InvalidSlotTransition { from, to })?;
        }
        self.translate();
        self.dirty.set(DirtyMarkers::SLOT, true);
        self.slot = to;
        Ok(())
    }

    /// Replaces all state flags and marks them dirty when the value changes.
    pub fn set_flags(&mut self, flags: StateFlags) {
        if self.flags == flags {
            return;
        }
        self.translate();
        self.dirty.set(DirtyMarkers::FLAGS, true);
        self.flags = flags;
    }

    /// Creates a new owned shared-data account with zero-filled data.
    pub fn new(lamports: u64, space: usize, owner: &Pubkey) -> Self {
        AccountBuilder::default()
            .lamports(lamports)
            .data(vec![0; space])
            .owner(*owner)
            .build()
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
    }
}

bitflags::bitflags! {
    /// Account state modifier flags.
    #[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
    #[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
    pub struct StateFlags: u8 {
        /// Executable account data.
        const EXECUTABLE = 1 << 0;
    }

    /// Bits that record which fields changed through `AccountSharedData`.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct DirtyMarkers: u8 {
        /// Owner changed.
        const OWNER    = 1 << 0;
        /// Lamports changed.
        const LAMPORTS = 1 << 1;
        /// Mode changed.
        const MODE     = 1 << 2;
        /// State flags changed.
        const FLAGS    = 1 << 3;
        /// Slot changed.
        const SLOT     = 1 << 4;
        /// Data bytes changed.
        const DATA     = 1 << 5;
    }
}

/// `wincode` codec for `StateFlags`, which is a `bitflags!` newtype and so
/// cannot use the derives. Routed through `bincode`/`serde`, which encodes the
/// single bits byte identically to a plain `u8`.
#[cfg(feature = "wincode")]
const _: () = {
    use core::mem::MaybeUninit;
    use wincode::{
        ReadError, ReadResult, SchemaRead, SchemaWrite, TypeMeta, WriteError, WriteResult,
        config::ConfigCore,
        io::{Reader, Writer},
    };

    // SAFETY: encodes exactly one byte; matches `TYPE_META` / `size_of`.
    unsafe impl<C: ConfigCore> SchemaWrite<C> for StateFlags {
        type Src = StateFlags;
        const TYPE_META: TypeMeta = TypeMeta::Static { size: 1, zero_copy: false };

        fn size_of(_: &Self::Src) -> WriteResult<usize> {
            Ok(1)
        }

        fn write(mut writer: impl Writer, src: &Self::Src) -> WriteResult<()> {
            let bytes = bincode::serialize(src).map_err(|_| WriteError::Custom("StateFlags"))?;
            writer.write(&bytes)?;
            Ok(())
        }
    }

    // SAFETY: consumes exactly one byte; matches `TYPE_META`.
    unsafe impl<'de, C: ConfigCore> SchemaRead<'de, C> for StateFlags {
        type Dst = StateFlags;
        const TYPE_META: TypeMeta = TypeMeta::Static { size: 1, zero_copy: false };

        fn read(mut reader: impl Reader<'de>, dst: &mut MaybeUninit<Self::Dst>) -> ReadResult<()> {
            let bytes = reader.take_array::<1>()?;
            dst.write(bincode::deserialize(&bytes).map_err(|_| ReadError::Custom("StateFlags"))?);
            Ok(())
        }
    }
};

/// Backing storage for `AccountSharedData`.
#[derive(PartialEq, Eq)]
pub enum CoWAccount {
    /// Borrowed image, a view into static backing buffer.
    Borrowed(BorrowedAccount),
    /// Heap-owned image.
    Owned(OwnedAccount),
}

impl Clone for CoWAccount {
    fn clone(&self) -> Self {
        match self {
            Borrowed(acc) => Self::Owned(acc.into()),
            Owned(acc) => Self::Owned(acc.clone()),
        }
    }
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
    pub fn reserve(&mut self, additional: usize) {
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
}

/// Mutually exclusive modes an account can occupy in the ephemeral rollup (ER).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
#[repr(u8)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "wincode", derive(wincode::SchemaRead, wincode::SchemaWrite))]
pub enum AccountMode {
    /// Empty account (not found on chain) used to avoid frequent chain syncs.
    #[default]
    Placeholder = 0,
    /// Not writable by users (exists on chain, but not delegated)
    ReadOnly = 1,
    /// Internal account used for sysvars, features, and precompiles.
    System,
    /// Account delegated to the current ER node instance.
    Delegated,
    /// Account that exists only inside the ER.
    Ephemeral,
    /// Temporary state during mode transitions (e.g. delegated -> readonly).
    Transient,
    /// Closed account that should be removed from storage.
    Closed = 255,
}

impl AccountMode {
    /// Returns `true` for modes that may be mutated by user programs.
    pub fn mutable(&self) -> bool {
        use AccountMode::*;
        matches!(self, Delegated | Ephemeral)
    }

    /// Returns `true` for modes whose state is authoritative in this engine.
    pub fn authoritative(&self) -> bool {
        use AccountMode::*;
        matches!(self, Delegated | Ephemeral | Transient)
    }
}

/// Read wrapper that retries borrowed account reads when a concurrent publish
/// changes the backing image.
pub struct AccountSeqLock {
    account: AccountSharedData,
    sequence: Option<u32>,
}

impl AccountSeqLock {
    /// Creates a read lock with the sequence that matches the current account view.
    pub fn new(account: AccountSharedData) -> Self {
        let mut sequence = None;
        if let Borrowed(ref acc) = account.cow {
            sequence.replace(acc.version);
        }
        Self { account, sequence }
    }

    /// Runs `reader` against a stable account image.
    ///
    /// For borrowed accounts, the sequence is checked after the read. If a
    /// writer published a new image meanwhile, the account view is reset to that
    /// active image and the read is retried.
    pub fn read<F, R>(&mut self, reader: F) -> R
    where
        F: Fn(&AccountSharedData) -> R,
    {
        loop {
            // sequence is always present for borrowed accounts
            let pre = self.sequence.unwrap_or_default();
            let result = reader(&self.account);
            match self.account.cow_mut() {
                Borrowed(acc) => {
                    let post = acc.sequence();
                    if pre == post {
                        return result;
                    }
                    // SAFETY: a changed sequence means the active image may have
                    // moved, so the borrowed view must be repointed before retrying.
                    unsafe { acc.reset() };
                    self.sequence = Some(acc.version);
                }
                Owned(_) => return result,
            }
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
    }
}

/// We only access AccountSharedData via transaction lock in the
/// execution layer or with a SeqLock semantics outside of execution
unsafe impl Sync for AccountSharedData {}
