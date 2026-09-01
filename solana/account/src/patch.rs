use core::fmt;

use solana_clock::Slot;
use solana_pubkey::Pubkey;

use crate::{AccountMode, AccountSharedData, OwnedAccount, WritableAccount};

const MAX_DATA_CHUNK_SIZE: usize = (u16::MAX - 256) as usize;

/// Failure to apply an account lifecycle or ordering patch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AccountPatchError {
    /// The requested account mode transition is not part of the lifecycle.
    #[error("invalid account mode transition: {from:?} -> {to:?}")]
    InvalidModeTransition {
        /// Current account mode.
        from: AccountMode,
        /// Requested account mode.
        to: AccountMode,
    },
    /// The requested slot neither advances nor accompanies a mode transition.
    #[error("invalid account slot transition: {from} -> {to}")]
    InvalidSlotTransition {
        /// Current account slot.
        from: Slot,
        /// Requested account slot.
        to: Slot,
    },
}

/// A bounded account-image patch.
#[cfg_attr(feature = "wincode", derive(wincode::SchemaRead, wincode::SchemaWrite))]
pub enum AccountFieldPatch {
    /// Replaces the lamport balance.
    Lamports(u64),
    /// Replaces the owner.
    Owner(Pubkey),
    /// Writes bytes starting at `offset`, extending the account data if needed.
    DataAt {
        /// Byte offset into the current data buffer.
        offset: usize,
        /// Bytes to write.
        data: Vec<u8>,
    },
    /// Replaces the account lifecycle.
    Lifecycle {
        /// Requested lifecycle mode.
        mode: AccountMode,
        /// Requested lifecycle slot.
        slot: Slot,
    },
    /// Resizes the data buffer to an exact length, zero-filling when growing.
    DataLen(usize),
}

impl AccountFieldPatch {
    /// Applies this patch to `account`.
    ///
    /// The account methods mark dirtiness and preserve the writable invariants.
    /// Invalid mode and slot transitions leave the account unchanged and return
    /// their transition context.
    pub fn apply(self, account: &mut AccountSharedData) -> Result<(), AccountPatchError> {
        match self {
            Self::Lamports(v) => account.set_lamports(v),
            Self::Owner(v) => account.set_owner(v),
            Self::Lifecycle { mode, slot } => return account.set_lifecycle(mode, slot),
            Self::DataAt { offset, data } => account.set_data_at(offset, &data),
            Self::DataLen(len) => account.resize(len, 0),
        }
        Ok(())
    }

    /// Decomposes an owned account into the ordered sequence of patches that
    /// reconstruct its non-flag fields: lamports, lifecycle, owner, the exact
    /// data length, then the data in `MAX_DATA_CHUNK_SIZE`-sized chunks.
    pub fn sequence(account: OwnedAccount) -> Vec<Self> {
        let mut sequence = Vec::with_capacity(5);
        sequence.push(Self::Lamports(account.core.lamports));
        sequence.push(Self::Lifecycle {
            mode: account.core.mode,
            slot: account.core.slot,
        });
        sequence.push(Self::Owner(account.core.owner));
        sequence.push(Self::DataLen(account.data.len()));
        let mut offset = 0;
        for data in account.data.chunks(MAX_DATA_CHUNK_SIZE) {
            sequence.push(Self::DataAt { offset, data: data.into() });
            offset += data.len();
        }
        sequence
    }
}

/// Concise, log-friendly rendering: scalar fields show their value, `DataAt`
/// shows only `offset+len` (never the raw bytes).
impl fmt::Debug for AccountFieldPatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lamports(v) => write!(f, "lamports={v}"),
            Self::Owner(v) => write!(f, "owner={v}"),
            Self::Lifecycle { mode, slot } => write!(f, "lifecycle={mode:?}@{slot}"),
            Self::DataAt { offset, data } => write!(f, "data@{offset}+{}", data.len()),
            Self::DataLen(len) => write!(f, "data_len={len}"),
        }
    }
}
