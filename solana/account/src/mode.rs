//! Account classification, local access, synchronization policy, and lifecycle transitions.

use solana_clock::Slot;

use crate::AccountPatchError;
use AccessMode::{ReadOnly, Writable};
use AccountKind::*;

const KIND_OFFSET: u32 = 0;
const ACCESS_OFFSET: u32 = 8;
const FLOW_OFFSET: u32 = 10;

const KIND_MASK: u16 = 0x00FF << KIND_OFFSET;
const ACCESS_MASK: u16 = 0x0003 << ACCESS_OFFSET;
const FLOW_MASK: u16 = 0x0003 << FLOW_OFFSET;

/// Classification that access and synchronization policy cannot determine.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AccountKind {
    /// Cached absence of an account on the base chain.
    Missing,
    /// Account state whose access and synchronization policy determine its behavior.
    Regular,
    /// Locally supplied program image rebuilt at startup instead of persisted.
    BuiltIn,
    /// Deletion tombstone retained until storage removes the account image.
    Closed = 3,
}

/// Encodes kind, access, and data flow into a 16-bit mode discriminant.
///
/// | Field    | Bit positions | Capacity   |
/// | -------- | ------------- | ---------- |
/// | Kind     | 0–7           | 256 values |
/// | Access   | 8–9           | 4 values   |
/// | Flow     | 10–11         | 4 values   |
/// | Reserved | 12–15         | 16 values  |
///
/// Reserved bits must be zero.
/// Little-endian bytes are `[kind, access | (flow << 2)]`.
const fn encode(kind: AccountKind, access: AccessMode, flow: DataFlow) -> u16 {
    ((kind as u16) << KIND_OFFSET)
        | ((access as u16) << ACCESS_OFFSET)
        | ((flow as u16) << FLOW_OFFSET)
}

/// Permission for ordinary program mutation in this engine.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AccessMode {
    /// Ordinary execution may not mutate the account.
    ReadOnly,
    /// Ordinary execution may mutate the account, subject to runtime checks.
    Writable,
}

/// Ongoing state synchronization between base and this ER.
/// Initial cloning and privileged lifecycle operations are separate.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DataFlow {
    /// No ongoing account-state synchronization with base.
    NoSync,
    /// Base supplies refreshed account state.
    BaseToEr,
    /// Local state participates in delegated commitment to base.
    ErToBase,
}

/// Side that supplies or maintains the account image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StateAuthority {
    /// The base chain supplies the account state.
    Base,
    /// This ER maintains the account state.
    Er,
}

/// Two-byte account mode whose variants declare kind, access, and synchronization policy.
/// Wincode and bincode encode the same bytes as its in-memory discriminant.
#[repr(u16)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(try_from = "[u8; 2]", into = "[u8; 2]"))]
pub enum AccountMode {
    /// Cached base-chain absence, awaiting base updates.
    #[default]
    Missing = encode(Missing, ReadOnly, DataFlow::BaseToEr),

    /// Read-only base mirror, including accounts delegated to another validator.
    Mirrored = encode(Regular, ReadOnly, DataFlow::BaseToEr),

    /// Active local delegation. The host establishes authority and the commit target.
    Delegated = encode(Regular, Writable, DataFlow::ErToBase),

    /// Protected local state awaiting completion of a delegation handoff.
    Undelegating = encode(Regular, ReadOnly, DataFlow::ErToBase),

    /// Writable ER state without direct delegated commitment to base.
    /// A counterpart at the same address may independently exist on base.
    Magic = encode(Regular, Writable, DataFlow::NoSync),

    /// Read-only, locally maintained state without base-chain synchronization.
    Runtime = encode(Regular, ReadOnly, DataFlow::NoSync),

    /// Read-only local program image, kept in memory without base-chain synchronization.
    BuiltIn = encode(BuiltIn, ReadOnly, DataFlow::NoSync),

    /// Deletion tombstone awaiting physical removal.
    Closed = encode(Closed, ReadOnly, DataFlow::NoSync),
}

/// A byte pair that does not encode a supported account mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid account mode bytes: {0:?}")]
pub struct InvalidAccountMode([u8; 2]);

impl AccountMode {
    /// Returns the remaining runtime/lifecycle classification.
    pub(crate) const fn kind(self) -> AccountKind {
        match ((self as u16) & KIND_MASK) >> KIND_OFFSET {
            0 => Missing,
            1 => Regular,
            2 => BuiltIn,
            3 => Closed,
            _ => unreachable!(),
        }
    }

    /// Returns ordinary local write permission.
    pub(crate) const fn access(self) -> AccessMode {
        match ((self as u16) & ACCESS_MASK) >> ACCESS_OFFSET {
            0 => ReadOnly,
            1 => Writable,
            _ => unreachable!(),
        }
    }

    /// Returns the ongoing account-state synchronization policy.
    pub(crate) const fn data_flow(self) -> DataFlow {
        match ((self as u16) & FLOW_MASK) >> FLOW_OFFSET {
            0 => DataFlow::NoSync,
            1 => DataFlow::BaseToEr,
            2 => DataFlow::ErToBase,
            _ => unreachable!(),
        }
    }

    /// Returns whether ordinary programs may mutate this account.
    pub const fn is_writable(self) -> bool {
        matches!(self.access(), Writable)
    }

    /// Returns who maintains the account image, or `None` for missing and closed accounts.
    pub(crate) const fn authority(self) -> Option<StateAuthority> {
        match (self.kind(), self.data_flow()) {
            (Missing | Closed, _) => None,
            (_, DataFlow::BaseToEr) => Some(StateAuthority::Base),
            (_, DataFlow::NoSync | DataFlow::ErToBase) => Some(StateAuthority::Er),
        }
    }

    /// Returns whether the ER is authoritative for this account's state.
    pub const fn is_er_owned(self) -> bool {
        matches!(self.authority(), Some(StateAuthority::Er))
    }

    /// Returns whether local state requires persistence.
    pub const fn requires_persistence(self) -> bool {
        matches!(self.kind(), Regular) && self.is_er_owned()
    }

    /// Validates lifecycle eligibility before slot freshness.
    pub(crate) fn validate_transition(
        self,
        to: Self,
        from_slot: Slot,
        to_slot: Slot,
    ) -> Result<(), AccountPatchError> {
        use AccountMode::*;

        let valid = match (self, to) {
            (Missing, Missing)
            | (Mirrored, Missing | Mirrored)
            | (Runtime, Runtime)
            | (BuiltIn, BuiltIn)
            | (Undelegating, Delegated) => to_slot > from_slot,
            (Missing, Mirrored | Runtime | BuiltIn | Delegated | Magic | Closed)
            | (Mirrored, Delegated | Magic | Closed)
            | (Delegated, Undelegating)
            | (Magic, Delegated | Closed)
            | (Undelegating, Missing | Mirrored | Closed) => to_slot >= from_slot,
            _ => return Err(AccountPatchError::InvalidModeTransition { from: self, to }),
        };
        if !valid {
            return Err(AccountPatchError::InvalidSlotTransition { from: from_slot, to: to_slot });
        }
        Ok(())
    }
}

impl From<AccountMode> for [u8; 2] {
    fn from(mode: AccountMode) -> Self {
        (mode as u16).to_le_bytes()
    }
}

impl TryFrom<[u8; 2]> for AccountMode {
    type Error = InvalidAccountMode;

    fn try_from(bytes: [u8; 2]) -> Result<Self, Self::Error> {
        match bytes {
            [0, 4] => Ok(Self::Missing),
            [1, 4] => Ok(Self::Mirrored),
            [1, 0] => Ok(Self::Runtime),
            [2, 0] => Ok(Self::BuiltIn),
            [1, 9] => Ok(Self::Delegated),
            [1, 1] => Ok(Self::Magic),
            [1, 8] => Ok(Self::Undelegating),
            [3, 0] => Ok(Self::Closed),
            _ => Err(InvalidAccountMode(bytes)),
        }
    }
}

#[cfg(feature = "wincode")]
mod codec {
    use super::AccountMode;
    use std::mem::MaybeUninit;
    use wincode::{
        ReadResult, SchemaRead, SchemaWrite, TypeMeta, WriteResult,
        config::ConfigCore,
        error::invalid_value,
        io::{Reader, Writer},
    };

    // SAFETY: each variant writes exactly two initialized discriminant bytes.
    unsafe impl<C: ConfigCore> SchemaWrite<C> for AccountMode {
        type Src = Self;
        const TYPE_META: TypeMeta = TypeMeta::Static { size: 2, zero_copy: false };

        fn size_of(_src: &Self) -> WriteResult<usize> {
            Ok(2)
        }

        fn write(writer: impl Writer, src: &Self) -> WriteResult<()> {
            <[u8; 2] as SchemaWrite<C>>::write(writer, &(*src).into())
        }
    }

    // SAFETY: exactly two bytes are consumed and dst is initialized only after validation succeeds.
    unsafe impl<'de, C: ConfigCore> SchemaRead<'de, C> for AccountMode {
        type Dst = Self;
        const TYPE_META: TypeMeta = TypeMeta::Static { size: 2, zero_copy: false };

        fn read(reader: impl Reader<'de>, dst: &mut MaybeUninit<Self>) -> ReadResult<()> {
            let bytes = <[u8; 2] as SchemaRead<'de, C>>::get(reader)?;
            let mode =
                AccountMode::try_from(bytes).map_err(|_| invalid_value("invalid account mode"))?;
            dst.write(mode);
            Ok(())
        }
    }
}
