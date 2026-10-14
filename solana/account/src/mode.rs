//! Account classification, local access, synchronization policy, and lifecycle transitions.

use solana_clock::Slot;

use crate::AccountPatchError;

/// Classification that access and synchronization policy cannot determine.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AccountKind {
    /// Cached absence of an account on the base chain.
    #[default]
    Uninit = 0,
    /// Ordinary account state, including mirrors, delegation, and independent ER state.
    Regular = 1,
    /// Internal runtime state, such as sysvars, features, and programs.
    System = 2,
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

/// Ongoing account-state synchronization between the base chain and the Magic Validator.
///
/// Magic names the validator here, independently of an account's mode.
///
/// Initial cloning, delegation-status observation, and privileged lifecycle
/// operations are separate. An outbound policy remains during handoff without
/// permitting ordinary writes or authorizing arbitrary new commits.
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
#[cfg_attr(feature = "serde", serde(try_from = "[u8; 2]", into = "[u8; 2]"))]
pub struct AccountMode {
    kind: AccountKind,
    /// Bit 0 is access; bits 1–2 are data flow. Other bits are reserved.
    bits: u8,
}

/// Encoded mode with an invalid tag, reserved bits, or unsupported combination.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid account mode bytes: {0:?}")]
pub struct InvalidAccountMode([u8; 2]);

// These constants retain the familiar spelling of the original enum variants.
#[allow(non_upper_case_globals)]
impl AccountMode {
    /// Cached base-chain absence, awaiting base updates.
    pub const Uninit: Self = Self::new(
        AccountKind::Uninit,
        AccessMode::ReadOnly,
        DataFlow::BaseToMagic,
    );

    /// Read-only base mirror, including accounts delegated to another validator.
    pub const Base: Self = Self::new(
        AccountKind::Regular,
        AccessMode::ReadOnly,
        DataFlow::BaseToMagic,
    );

    /// Active local delegation. The host establishes authority and the commit target.
    pub const Delegated: Self = Self::new(
        AccountKind::Regular,
        AccessMode::Writable,
        DataFlow::MagicToBase,
    );

    /// Protected local state awaiting completion of a delegation handoff.
    pub const Undelegating: Self = Self::new(
        AccountKind::Regular,
        AccessMode::ReadOnly,
        DataFlow::MagicToBase,
    );

    /// Writable ER state without direct delegated commitment to base.
    /// A counterpart at the same address may independently exist on base.
    pub const Magic: Self = Self::new(AccountKind::Regular, AccessMode::Writable, DataFlow::None);

    /// Runtime-maintained state, such as locally generated sysvars.
    pub const System: Self = Self::new(AccountKind::System, AccessMode::ReadOnly, DataFlow::None);

    /// Runtime account whose image is refreshed from base.
    pub const MirroredSystem: Self = Self::new(
        AccountKind::System,
        AccessMode::ReadOnly,
        DataFlow::BaseToMagic,
    );

    /// Deletion tombstone awaiting physical removal.
    pub const Closed: Self = Self::new(AccountKind::Closed, AccessMode::ReadOnly, DataFlow::None);

    // Only named constants assemble modes; decoding separately validates the complete encoding.
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
            0 => AccessMode::ReadOnly,
            _ => AccessMode::Writable,
        }
    }

    /// Returns the ongoing account-state synchronization policy.
    pub const fn data_flow(self) -> DataFlow {
        match (self.bits >> 1) & 3 {
            0 => DataFlow::None,
            1 => DataFlow::BaseToMagic,
            2 => DataFlow::MagicToBase,
            _ => unreachable!(),
        }
    }

    /// Returns whether ordinary programs may mutate this account.
    pub const fn is_writable(self) -> bool {
        matches!(self.access(), AccessMode::Writable)
    }

    /// Returns whether this mode represents active local delegation.
    pub const fn is_locally_delegated(self) -> bool {
        matches!(
            (self.kind, self.access(), self.data_flow()),
            (
                AccountKind::Regular,
                AccessMode::Writable,
                DataFlow::MagicToBase
            )
        )
    }

    /// Returns whether local delegation is locked pending handoff completion.
    pub const fn is_undelegating(self) -> bool {
        matches!(
            (self.kind, self.access(), self.data_flow()),
            (
                AccountKind::Regular,
                AccessMode::ReadOnly,
                DataFlow::MagicToBase
            )
        )
    }

    /// Returns whether the engine must retain and protect the local state.
    pub const fn is_authoritative(self) -> bool {
        matches!(
            (self.kind, self.data_flow()),
            (AccountKind::Regular, DataFlow::None | DataFlow::MagicToBase)
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
            (Self::Uninit, Self::Uninit)
            | (Self::Base, Self::Base | Self::Uninit)
            | (Self::System | Self::MirroredSystem, Self::System | Self::MirroredSystem)
            | (Self::Undelegating, Self::Delegated) => true,
            (
                Self::Uninit,
                Self::Base
                | Self::System
                | Self::MirroredSystem
                | Self::Delegated
                | Self::Magic
                | Self::Closed,
            )
            | (Self::Base, Self::Delegated | Self::Magic | Self::Closed)
            | (Self::Delegated, Self::Undelegating)
            | (Self::Magic, Self::Closed)
            | (Self::Undelegating, Self::Base | Self::Uninit | Self::Closed) => false,
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
        Self::Uninit
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

impl From<AccountMode> for [u8; 2] {
    fn from(mode: AccountMode) -> Self {
        [mode.kind as u8, mode.bits]
    }
}

impl TryFrom<[u8; 2]> for AccountMode {
    type Error = InvalidAccountMode;

    fn try_from(bytes: [u8; 2]) -> Result<Self, Self::Error> {
        match bytes {
            [0, 2] => Ok(Self::Uninit),
            [1, 2] => Ok(Self::Base),
            [1, 5] => Ok(Self::Delegated),
            [1, 4] => Ok(Self::Undelegating),
            [1, 1] => Ok(Self::Magic),
            [2, 0] => Ok(Self::System),
            [2, 2] => Ok(Self::MirroredSystem),
            [255, 0] => Ok(Self::Closed),
            _ => Err(InvalidAccountMode(bytes)),
        }
    }
}

const _: () = assert!(size_of::<AccountKind>() == 1);
const _: () = assert!(size_of::<AccessMode>() == 1);
const _: () = assert!(size_of::<DataFlow>() == 1);
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

    // SAFETY: the codec always writes exactly two bytes, without claiming zero-copy eligibility.
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
