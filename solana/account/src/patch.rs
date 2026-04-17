use solana_clock::Slot;
use solana_pubkey::Pubkey;

use crate::{AccountMode, AccountSharedData, WritableAccount, cow::StateFlags};

/// A single-field account patch.
pub enum AccountFieldPatch {
    /// Replaces the lamport balance.
    Lamports(u64),
    /// Replaces the owner.
    Owner(Pubkey),
    /// Replaces the data with new bytes.
    Data(Vec<u8>),
    /// Writes bytes starting at `offset`, extending the account data if needed.
    DataAt {
        /// Byte offset into the current data buffer.
        offset: usize,
        /// Bytes to write.
        data: Vec<u8>,
    },
    /// Replaces the slot.
    Slot(Slot),
    /// Replaces the account mode.
    Mode(AccountMode),
    /// Sets or clears a state flag.
    Flag {
        /// Flag to update.
        flag: StateFlags,
        /// Whether to set the flag.
        val: bool,
    },
}

impl AccountFieldPatch {
    /// Applies this patch to `account`.
    ///
    /// The account methods mark dirtiness and preserve the writable invariants.
    pub fn apply(self, account: &mut AccountSharedData) {
        match self {
            Self::Lamports(v) => account.set_lamports(v),
            Self::Slot(v) => account.set_slot(v),
            Self::Owner(v) => account.set_owner(v),
            Self::Flag { flag, val } => account.set_flag(flag, val),
            Self::Mode(v) => account.set_mode(v),
            Self::Data(v) => account.set_data(v),
            Self::DataAt { offset, data } => account.set_data_at(offset, &data),
        }
    }
}
