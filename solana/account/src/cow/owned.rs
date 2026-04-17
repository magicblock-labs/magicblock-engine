use super::borrowed::{AccountHeader, DataHeader, STATIC_SIZE};
use super::{ALIGNMENT, StateFlags};
use crate::cow::AccountCore;
use crate::cow::borrowed::IMAGE_OFFSET;
use crate::{Account, StorageUnit};
use solana_clock::Slot;
use solana_pubkey::Pubkey;
use std::{ptr::NonNull, sync::Arc};

/// Mutually exclusive modes an account can occupy in the engine.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum AccountMode {
    /// Default account state. Not writable by users.
    #[default]
    ReadOnly,
    /// Empty account used to avoid frequent chain syncs.
    Placeholder,
    /// Privileged account that can sign internal transactions.
    Authority,
    /// Internal account used for sysvars, features, and precompiles.
    System,
    /// Account delegated to the current engine instance.
    Delegated,
    /// Account that exists only inside the engine.
    Ephemeral,
    /// Temporary state during mode transitions.
    Transient,
    /// Closed account that should be removed from storage.
    Closed,
}

/// Heap-backed account, used after promotion from borrowed or direct construction.
#[derive(Clone, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct OwnedAccount {
    /// Core account fields.
    pub(crate) core: AccountCore,
    /// Heap-owned data buffer.
    pub(crate) data: Arc<Vec<u8>>,
}

impl OwnedAccount {
    /// Returns the exact storage units needed to serialize this account.
    pub fn units(&self) -> u32 {
        self.allocation() * 2 + IMAGE_OFFSET as u32
    }

    /// Returns the storage units needed for one image, rounded up to alignment.
    fn allocation(&self) -> u32 {
        (STATIC_SIZE + self.data.len()).div_ceil(ALIGNMENT) as u32
    }

    /// Writes the account into a buffer sized by `units`.
    ///
    /// # Safety
    ///
    /// `buf` must be exactly `units()` storage units long.
    /// `pubkey` is written into the image prefix so borrowed iteration can
    /// recover the full account key without consulting the index.
    pub unsafe fn serialize(&self, buf: &mut [StorageUnit], pubkey: &Pubkey) {
        let ptr = NonNull::new_unchecked(buf.as_mut_ptr());
        debug_assert_eq!(self.units() as usize, buf.len());

        unsafe fn write<U, T: Sized>(ptr: NonNull<U>, v: T) -> NonNull<T> {
            ptr.cast().write(v);
            ptr.cast().add(1)
        }

        let allocation = self.allocation();
        let ptr = write(ptr, AccountHeader::new(allocation));
        // The image prefix stores the account pubkey for later iteration.
        let ptr = write(ptr, *pubkey);
        let ptr = write(ptr, self.core);
        let len = self.data.len();
        let ptr = write(ptr, DataHeader::new(len as u32, allocation)).cast();
        self.data.as_ptr().copy_to_nonoverlapping(ptr.as_ptr(), len);
    }

    /// Returns the owner pubkey.
    pub fn owner(&self) -> Pubkey {
        self.core.owner
    }
}

/// Builder for an owned account representation.
///
/// Use this when the account does not start from a borrowed external buffer.
#[derive(Default)]
pub struct AccountBuilder {
    /// Account under construction.
    acc: OwnedAccount,
}

impl AccountBuilder {
    /// Sets the lamport balance.
    pub fn lamports(mut self, lamports: u64) -> Self {
        self.acc.core.lamports = lamports;
        self
    }

    /// Sets the data buffer.
    pub fn data(mut self, data: impl Into<Arc<Vec<u8>>>) -> Self {
        self.acc.data = data.into();
        self
    }

    /// Sets the owner.
    pub fn owner(mut self, owner: Pubkey) -> Self {
        self.acc.core.owner = owner;
        self
    }

    /// Sets the executable flag.
    pub fn executable(mut self, executable: bool) -> Self {
        self.acc.core.flags.set(StateFlags::EXECUTABLE, executable);
        self
    }

    /// Sets the compressed flag.
    pub fn compressed(mut self, compressed: bool) -> Self {
        self.acc.core.flags.set(StateFlags::COMPRESSED, compressed);
        self
    }

    /// Sets the on chain slot.
    pub fn slot(mut self, slot: Slot) -> Self {
        self.acc.core.slot = slot;
        self
    }

    /// Finishes building the owned account.
    pub fn build(self) -> OwnedAccount {
        self.acc
    }
}

impl From<Account> for OwnedAccount {
    fn from(value: Account) -> Self {
        AccountBuilder::default()
            .lamports(value.lamports)
            .data(value.data)
            .owner(value.owner)
            .executable(value.executable)
            .build()
    }
}
