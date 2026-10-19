//! Account classification, local access, synchronization policy, and lifecycle transitions.

use solana_clock::Slot;

use crate::AccountPatchError;
use AccessMode::{ReadOnly, Writable};
use AccountKind::*;

/// Classification that access and synchronization policy cannot determine.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountKind {
    /// Cached absence of an account on the base chain.
    Missing,
    /// Ordinary account state, including mirrors, delegation, and independent ER state.
    Regular,
    /// Internal runtime state, such as sysvars, features, and programs.
    Runtime,
    /// Deletion tombstone retained until storage removes the account image.
    Closed,
}

impl AccountKind {
    const fn encode(self, access: AccessMode, flow: DataFlow) -> u16 {
        // Keep raw mode bytes ordered as [kind, access | flow << 1].
        u16::from_ne_bytes([self as u8, (access as u8) | ((flow as u8) << 1)])
    }
}

/// Permission for ordinary program mutation in this engine.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessMode {
    /// Ordinary execution may not mutate the account.
    ReadOnly,
    /// Ordinary execution may mutate the account, subject to runtime checks.
    Writable,
}

/// Ongoing state synchronization with base; Magic names the validator endpoint.
/// Initial cloning and privileged lifecycle operations are separate.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataFlow {
    /// No ongoing account-state synchronization with base.
    NoSync,
    /// Base supplies refreshed account state.
    BaseToMagic,
    /// Local state participates in delegated commitment to base.
    MagicToBase,
}

/// Side that supplies or maintains the account image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateAuthority {
    /// The base chain supplies the account state.
    Base,
    /// This validator maintains the account state, including local runtime state.
    Magic,
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
    Missing = Missing.encode(ReadOnly, DataFlow::BaseToMagic),

    /// Read-only base mirror, including accounts delegated to another validator.
    Base = Regular.encode(ReadOnly, DataFlow::BaseToMagic),

    /// Active local delegation. The host establishes authority and the commit target.
    Delegated = Regular.encode(Writable, DataFlow::MagicToBase),

    /// Protected local state awaiting completion of a delegation handoff.
    Undelegating = Regular.encode(ReadOnly, DataFlow::MagicToBase),

    /// Writable ER state without direct delegated commitment to base.
    /// A counterpart at the same address may independently exist on base.
    Magic = Regular.encode(Writable, DataFlow::NoSync),

    /// Runtime-maintained state, such as locally generated sysvars.
    Runtime = Runtime.encode(ReadOnly, DataFlow::NoSync),

    /// Runtime account whose image is refreshed from base.
    MirroredRuntime = Runtime.encode(ReadOnly, DataFlow::BaseToMagic),

    /// Deletion tombstone awaiting physical removal.
    Closed = Closed.encode(ReadOnly, DataFlow::NoSync),
}

/// A byte pair that does not encode a supported account mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid account mode bytes: {0:?}")]
pub struct InvalidAccountMode([u8; 2]);

#[allow(unused_qualifications)]
impl AccountMode {
    /// Returns the remaining runtime/lifecycle classification.
    pub const fn kind(self) -> AccountKind {
        match (self as u16).to_ne_bytes()[0] {
            0 => AccountKind::Missing,
            1 => AccountKind::Regular,
            2 => AccountKind::Runtime,
            3 => AccountKind::Closed,
            _ => unreachable!(),
        }
    }

    /// Returns ordinary local write permission.
    pub const fn access(self) -> AccessMode {
        match (self as u16).to_ne_bytes()[1] & 1 {
            0 => ReadOnly,
            _ => Writable,
        }
    }

    /// Returns the ongoing account-state synchronization policy.
    pub const fn data_flow(self) -> DataFlow {
        match ((self as u16).to_ne_bytes()[1] >> 1) & 3 {
            0 => DataFlow::NoSync,
            1 => DataFlow::BaseToMagic,
            2 => DataFlow::MagicToBase,
            _ => unreachable!(),
        }
    }

    /// Returns whether ordinary programs may mutate this account.
    pub const fn is_writable(self) -> bool {
        matches!(self.access(), Writable)
    }

    /// Returns who maintains the account image, or `None` for missing and closed accounts.
    pub const fn authority(self) -> Option<StateAuthority> {
        match (self.kind(), self.data_flow()) {
            (AccountKind::Missing | AccountKind::Closed, _) => None,
            (_, DataFlow::BaseToMagic) => Some(StateAuthority::Base),
            (_, DataFlow::NoSync | DataFlow::MagicToBase) => Some(StateAuthority::Magic),
        }
    }

    /// Returns whether local state requires persistence. Runtime state is managed separately.
    pub const fn requires_persistence(self) -> bool {
        matches!(self.kind(), AccountKind::Regular)
            && matches!(self.authority(), Some(StateAuthority::Magic))
    }

    /// Returns whether a privileged operation may apply `to` at `to_slot`.
    pub fn allows_transition(self, to: Self, from_slot: Slot, to_slot: Slot) -> bool {
        self.validate_transition(to, from_slot, to_slot).is_ok()
    }

    /// Validates lifecycle eligibility before slot freshness.
    pub(crate) fn validate_transition(
        self,
        to: Self,
        from_slot: Slot,
        to_slot: Slot,
    ) -> Result<(), AccountPatchError> {
        let require_newer = match (self, to) {
            (Self::Missing, Self::Missing)
            | (Self::Base, Self::Base | Self::Missing)
            | (Self::Runtime | Self::MirroredRuntime, Self::Runtime | Self::MirroredRuntime)
            | (Self::Undelegating, Self::Delegated) => true,
            (
                Self::Missing,
                Self::Base
                | Self::Runtime
                | Self::MirroredRuntime
                | Self::Delegated
                | Self::Magic
                | Self::Closed,
            )
            | (Self::Base, Self::Delegated | Self::Magic | Self::Closed)
            | (Self::Delegated, Self::Undelegating)
            | (Self::Magic, Self::Closed)
            | (Self::Undelegating, Self::Base | Self::Missing | Self::Closed) => false,
            _ => return Err(AccountPatchError::InvalidModeTransition { from: self, to }),
        };
        if to_slot < from_slot || (require_newer && to_slot == from_slot) {
            return Err(AccountPatchError::InvalidSlotTransition { from: from_slot, to: to_slot });
        }
        Ok(())
    }
}

impl From<AccountMode> for [u8; 2] {
    fn from(mode: AccountMode) -> Self {
        (mode as u16).to_ne_bytes()
    }
}

impl TryFrom<[u8; 2]> for AccountMode {
    type Error = InvalidAccountMode;

    fn try_from(bytes: [u8; 2]) -> Result<Self, Self::Error> {
        match bytes {
            [0, 2] => Ok(Self::Missing),
            [1, 2] => Ok(Self::Base),
            [2, 0] => Ok(Self::Runtime),
            [1, 5] => Ok(Self::Delegated),
            [1, 1] => Ok(Self::Magic),
            [1, 4] => Ok(Self::Undelegating),
            [2, 2] => Ok(Self::MirroredRuntime),
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
