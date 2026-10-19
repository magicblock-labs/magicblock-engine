//! Account classification, local access, synchronization policy, and lifecycle transitions.

use solana_clock::Slot;

use crate::AccountPatchError;
use AccessMode::{ReadOnly, Writable};
use AccountKind::*;

/// Classification that access and synchronization policy cannot determine.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AccountKind {
    /// Cached absence of an account on the base chain.
    Missing,
    /// Ordinary account state, including mirrors, delegation, and independent ER state.
    Regular,
    /// Internal runtime state, such as sysvars, features, and programs.
    Runtime,
    /// Deletion tombstone retained until storage removes the account image.
    Closed,
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
    u16::from_le_bytes([kind as u8, (access as u8) | ((flow as u8) << 2)])
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

    /// Runtime-maintained state, such as locally generated sysvars.
    Runtime = encode(Runtime, ReadOnly, DataFlow::NoSync),

    /// Deletion tombstone awaiting physical removal.
    Closed = encode(Closed, ReadOnly, DataFlow::NoSync),
}

/// A byte pair that does not encode a supported account mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid account mode bytes: {0:?}")]
pub struct InvalidAccountMode([u8; 2]);

#[allow(unused_qualifications)]
impl AccountMode {
    /// Returns the remaining runtime/lifecycle classification.
    pub(crate) const fn kind(self) -> AccountKind {
        match (self as u16) & 0x00FF {
            0 => AccountKind::Missing,
            1 => AccountKind::Regular,
            2 => AccountKind::Runtime,
            3 => AccountKind::Closed,
            _ => unreachable!(),
        }
    }

    /// Returns ordinary local write permission.
    pub(crate) const fn access(self) -> AccessMode {
        match ((self as u16) & 0x0300) >> 8 {
            0 => ReadOnly,
            1 => Writable,
            _ => unreachable!(),
        }
    }

    /// Returns the ongoing account-state synchronization policy.
    pub(crate) const fn data_flow(self) -> DataFlow {
        match ((self as u16) & 0x0C00) >> 10 {
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
            (AccountKind::Missing | AccountKind::Closed, _) => None,
            (_, DataFlow::BaseToEr) => Some(StateAuthority::Base),
            (_, DataFlow::NoSync | DataFlow::ErToBase) => Some(StateAuthority::Er),
        }
    }

    /// Returns whether local state requires persistence.
    pub const fn requires_persistence(self) -> bool {
        matches!(self.kind(), AccountKind::Regular)
            && matches!(self.authority(), Some(StateAuthority::Er))
    }

    /// Validates lifecycle eligibility before slot freshness.
    pub(crate) fn validate_transition(
        self,
        to: Self,
        from_slot: Slot,
        to_slot: Slot,
    ) -> Result<(), AccountPatchError> {
        use AccountMode::*;

        enum TransitionRule {
            Forbidden,
            /// The lifecycle change may use the current image's slot.
            SameOrNewer,
            /// A fresh observation is required, not another update at the same slot.
            Newer,
        }
        use TransitionRule::*;

        let rule = match self {
            Missing => match to {
                Missing => Newer,
                Mirrored | Runtime | Delegated | Magic | Closed => SameOrNewer,
                Undelegating => Forbidden,
            },
            Mirrored => match to {
                Missing | Mirrored => Newer,
                Delegated | Magic | Closed => SameOrNewer,
                Runtime | Undelegating => Forbidden,
            },
            Runtime => match to {
                Runtime => Newer,
                Missing | Mirrored | Delegated | Magic | Undelegating | Closed => Forbidden,
            },
            Delegated => match to {
                Undelegating => SameOrNewer,
                Missing | Mirrored | Runtime | Delegated | Magic | Closed => Forbidden,
            },
            Magic => match to {
                Closed => SameOrNewer,
                Missing | Mirrored | Runtime | Delegated | Magic | Undelegating => Forbidden,
            },
            Undelegating => match to {
                Missing | Mirrored | Closed => SameOrNewer,
                Delegated => Newer,
                Runtime | Magic | Undelegating => Forbidden,
            },
            Closed => Forbidden,
        };

        let valid = match rule {
            Forbidden => return Err(AccountPatchError::InvalidModeTransition { from: self, to }),
            SameOrNewer => to_slot >= from_slot,
            Newer => to_slot > from_slot,
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
            [2, 0] => Ok(Self::Runtime),
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
