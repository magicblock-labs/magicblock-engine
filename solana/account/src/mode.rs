//! Account classification, local access, synchronization policy, and lifecycle transitions.

use solana_clock::Slot;

use crate::AccountPatchError;
use AccessMode::{ReadOnly, Writable};
use DataFlow::{BaseToMagic, MagicToBase};

/// Classification that access and synchronization policy cannot determine.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AccountKind {
    /// Cached absence of an account on the base chain.
    #[default]
    Missing = 0,
    /// Ordinary account state, including mirrors, delegation, and independent ER state.
    Regular = 1,
    /// Internal runtime state, such as sysvars, features, and programs.
    Runtime = 2,
    /// Deletion tombstone retained until storage removes the account image.
    Closed = 255,
}

/// Permission for ordinary program mutation in this engine.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AccessMode {
    /// Ordinary execution may not mutate the account.
    #[default]
    ReadOnly = 0,
    /// Ordinary execution may mutate the account, subject to runtime checks.
    Writable = 1,
}

/// Ongoing state synchronization with base; Magic names the validator endpoint.
/// Initial cloning and privileged lifecycle operations are separate.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DataFlow {
    /// No ongoing account-state synchronization with base.
    #[default]
    None = 0,
    /// Base supplies refreshed account state.
    BaseToMagic = 1,
    /// Local state participates in delegated commitment to base.
    MagicToBase = 2,
}

/// Two-byte account mode, exposed through named constants and typed getters.
#[repr(C)]
#[derive(Clone, Copy, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
pub struct AccountMode {
    kind: AccountKind,
    /// Bit 0 is access; bits 1–2 are data flow. Other bits are reserved.
    bits: u8,
}

/// A byte that does not encode a supported account mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid account mode byte: {0}")]
pub struct InvalidAccountMode(u8);

// These constants retain the familiar spelling of the original enum variants.
#[allow(non_upper_case_globals)]
impl AccountMode {
    /// Cached base-chain absence, awaiting base updates.
    pub const Missing: Self = Self::new(AccountKind::Missing, ReadOnly, BaseToMagic);

    /// Read-only base mirror, including accounts delegated to another validator.
    pub const Base: Self = Self::new(AccountKind::Regular, ReadOnly, BaseToMagic);

    /// Active local delegation. The host establishes authority and the commit target.
    pub const Delegated: Self = Self::new(AccountKind::Regular, Writable, MagicToBase);

    /// Protected local state awaiting completion of a delegation handoff.
    pub const Undelegating: Self = Self::new(AccountKind::Regular, ReadOnly, MagicToBase);

    /// Writable ER state without direct delegated commitment to base.
    /// A counterpart at the same address may independently exist on base.
    pub const Magic: Self = Self::new(AccountKind::Regular, Writable, DataFlow::None);

    /// Runtime-maintained state, such as locally generated sysvars.
    pub const Runtime: Self = Self::new(AccountKind::Runtime, ReadOnly, DataFlow::None);

    /// Runtime account whose image is refreshed from base.
    pub const MirroredRuntime: Self = Self::new(AccountKind::Runtime, ReadOnly, BaseToMagic);

    /// Deletion tombstone awaiting physical removal.
    pub const Closed: Self = Self::new(AccountKind::Closed, ReadOnly, DataFlow::None);
}

impl AccountMode {
    const fn new(kind: AccountKind, access: AccessMode, flow: DataFlow) -> Self {
        Self {
            kind,
            bits: (access as u8) | ((flow as u8) << 1),
        }
    }

    /// Returns the remaining runtime/lifecycle classification.
    pub const fn kind(self) -> AccountKind {
        self.kind
    }

    /// Returns ordinary local write permission.
    pub const fn access(self) -> AccessMode {
        match self.bits & 1 {
            0 => ReadOnly,
            _ => Writable,
        }
    }

    /// Returns the ongoing account-state synchronization policy.
    pub const fn data_flow(self) -> DataFlow {
        match (self.bits >> 1) & 3 {
            0 => DataFlow::None,
            1 => BaseToMagic,
            2 => MagicToBase,
            _ => unreachable!(),
        }
    }

    /// Returns whether ordinary programs may mutate this account.
    pub const fn is_writable(self) -> bool {
        matches!(self.access(), Writable)
    }

    /// Returns whether the engine must retain and protect the local state.
    pub const fn is_authoritative(self) -> bool {
        matches!(
            (self.kind, self.data_flow()),
            (AccountKind::Regular, DataFlow::None | MagicToBase)
        )
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

impl Default for AccountMode {
    fn default() -> Self {
        Self::Missing
    }
}

impl std::fmt::Debug for AccountMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountMode")
            .field("kind", &self.kind())
            .field("access", &self.access())
            .field("data_flow", &self.data_flow())
            .finish()
    }
}

impl From<AccountMode> for u8 {
    fn from(mode: AccountMode) -> Self {
        match mode {
            AccountMode::Missing => 0,
            AccountMode::Base => 1,
            AccountMode::Runtime => 2,
            AccountMode::Delegated => 3,
            AccountMode::Magic => 4,
            AccountMode::Undelegating => 5,
            AccountMode::MirroredRuntime => 6,
            AccountMode::Closed => 255,
            _ => unreachable!(),
        }
    }
}

impl TryFrom<u8> for AccountMode {
    type Error = InvalidAccountMode;

    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        match byte {
            0 => Ok(Self::Missing),
            1 => Ok(Self::Base),
            2 => Ok(Self::Runtime),
            3 => Ok(Self::Delegated),
            4 => Ok(Self::Magic),
            5 => Ok(Self::Undelegating),
            6 => Ok(Self::MirroredRuntime),
            255 => Ok(Self::Closed),
            _ => Err(InvalidAccountMode(byte)),
        }
    }
}

const _: () = assert!(size_of::<AccountMode>() == 2);

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

    // SAFETY: the codec always writes exactly one byte, without claiming zero-copy eligibility.
    unsafe impl<C: ConfigCore> SchemaWrite<C> for AccountMode {
        type Src = Self;
        const TYPE_META: TypeMeta = TypeMeta::Static { size: 1, zero_copy: false };

        fn size_of(_src: &Self) -> WriteResult<usize> {
            Ok(1)
        }

        fn write(writer: impl Writer, src: &Self) -> WriteResult<()> {
            <u8 as SchemaWrite<C>>::write(writer, &(*src).into())
        }
    }

    // SAFETY: exactly one byte is consumed and dst is initialized only after validation succeeds.
    unsafe impl<'de, C: ConfigCore> SchemaRead<'de, C> for AccountMode {
        type Dst = Self;
        const TYPE_META: TypeMeta = TypeMeta::Static { size: 1, zero_copy: false };

        fn read(reader: impl Reader<'de>, dst: &mut MaybeUninit<Self>) -> ReadResult<()> {
            let byte = <u8 as SchemaRead<'de, C>>::get(reader)?;
            let mode =
                AccountMode::try_from(byte).map_err(|_| invalid_value("invalid account mode"))?;
            dst.write(mode);
            Ok(())
        }
    }
}
