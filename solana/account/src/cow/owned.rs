use super::borrowed::{AccountHeader, DataHeader, STATIC_SIZE};
use super::{ALIGNMENT, StateFlags};
use crate::cow::AccountCore;
use crate::cow::borrowed::IMAGE_OFFSET;
use crate::{Account, AccountMode, AccountSharedData, StorageUnit};
use solana_clock::Slot;
use solana_pubkey::Pubkey;
use std::{ptr::NonNull, sync::Arc};

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

        fn write<U, T: Sized>(ptr: NonNull<U>, v: T) -> NonNull<T> {
            // SAFETY: `serialize` requires a buffer sized for the full layout.
            unsafe {
                ptr.cast().write(v);
                ptr.cast().add(1)
            }
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

    /// Tests the exact account mode without grouping modes by mutability.
    pub fn is(&self, mode: AccountMode) -> bool {
        self.core.mode == mode
    }

    /// Returns the owner pubkey.
    pub fn owner(&self) -> Pubkey {
        self.core.owner
    }

    /// Returns the lamport balance.
    pub fn lamports(&self) -> u64 {
        self.core.lamports
    }

    /// Returns the account's exact lifecycle mode.
    pub fn mode(&self) -> AccountMode {
        self.core.mode
    }

    /// Returns the account's on-chain slot.
    pub fn slot(&self) -> u64 {
        self.core.slot
    }

    /// Returns the account modifier flags.
    pub fn flags(&self) -> StateFlags {
        self.core.flags
    }

    /// Returns the account data.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

/// Builder for an owned account representation.
///
/// Use this when the account does not start from a borrowed external buffer.
#[derive(Default, Clone)]
pub struct AccountBuilder(OwnedAccount);

impl AccountBuilder {
    /// Sets the lamport balance.
    pub fn lamports(mut self, lamports: u64) -> Self {
        self.0.core.lamports = lamports;
        self
    }

    /// Sets the data buffer.
    pub fn data(mut self, data: impl Into<Arc<Vec<u8>>>) -> Self {
        self.0.data = data.into();
        self
    }

    /// Sets the owner.
    pub fn owner(mut self, owner: Pubkey) -> Self {
        self.0.core.owner = owner;
        self
    }

    /// Sets the account persistence mode of the account
    pub fn mode(mut self, mode: AccountMode) -> Self {
        self.0.core.mode = mode;
        self
    }

    /// Sets the executable flag.
    pub fn executable(mut self, executable: bool) -> Self {
        self.0.core.flags.set(StateFlags::EXECUTABLE, executable);
        self
    }

    /// Sets the on chain slot.
    pub fn slot(mut self, slot: Slot) -> Self {
        self.0.core.slot = slot;
        self
    }

    /// Borrows the account under construction.
    pub fn read(&self) -> &OwnedAccount {
        &self.0
    }

    /// Finishes building the owned account.
    pub fn build<A: From<OwnedAccount>>(self) -> A {
        self.0.into()
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

impl From<Account> for AccountBuilder {
    fn from(value: Account) -> Self {
        Self(value.into())
    }
}

impl From<AccountSharedData> for AccountBuilder {
    fn from(value: AccountSharedData) -> Self {
        Self(value.owned())
    }
}
