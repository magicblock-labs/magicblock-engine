use core::fmt;

use solana_clock::Slot;
use solana_pubkey::Pubkey;

use crate::{AccountMode, AccountSharedData, OwnedAccount, WritableAccount, cow::StateFlags};

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

/// A single-field account patch.
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
            Self::Slot(v) => return account.set_slot(v),
            Self::Owner(v) => account.set_owner(v),
            Self::Flag { flag, val } => account.set_flag(flag, val),
            Self::Mode(v) => return account.set_mode(v),
            Self::DataAt { offset, data } => account.set_data_at(offset, &data),
            Self::DataLen(len) => account.resize(len, 0),
        }
        Ok(())
    }

    /// Decomposes an owned account into the ordered sequence of patches that
    /// reconstruct it: lamports, mode, slot, owner, every flag, the exact data
    /// length, then the data in `MAX_DATA_CHUNK_SIZE`-sized chunks.
    ///
    /// Mode precedes slot so consumers can distinguish an equal-slot mode
    /// transition from a duplicate replacement by inspecting the mode dirty
    /// marker when they apply the slot patch.
    pub fn sequence(account: OwnedAccount) -> Vec<Self> {
        let mut sequence = Vec::with_capacity(6);
        sequence.push(Self::Lamports(account.core.lamports));
        sequence.push(Self::Mode(account.core.mode));
        sequence.push(Self::Slot(account.core.slot));
        sequence.push(Self::Owner(account.core.owner));
        for flag in StateFlags::all().iter() {
            let val = account.core.flags.contains(flag);
            sequence.push(Self::Flag { flag, val });
        }
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
            Self::Slot(v) => write!(f, "slot={v}"),
            Self::Mode(v) => write!(f, "mode={v:?}"),
            Self::Flag { flag, val } => write!(f, "flag={flag:?}={val}"),
            Self::DataAt { offset, data } => write!(f, "data@{offset}+{}", data.len()),
            Self::DataLen(len) => write!(f, "data_len={len}"),
        }
    }
}
