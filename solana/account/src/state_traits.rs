//! Typed bincode access to account data.

use {
    crate::{Account, AccountSharedData},
    bincode::ErrorKind,
    solana_instruction_error::InstructionError,
    std::cell::Ref,
};

/// Reads and writes typed account state through a mutable account handle.
pub trait StateMut<T> {
    /// Deserializes the account data as `T`.
    fn state(&self) -> Result<T, InstructionError>;

    /// Serializes `state` into the existing account data buffer.
    fn set_state(&mut self, state: &T) -> Result<(), InstructionError>;
}

/// Reads and writes typed account state through a shared handle.
///
/// Writing through `Ref<'_, AccountSharedData>` is rejected.
pub trait State<T> {
    /// Deserializes the account data as `T`.
    fn state(&self) -> Result<T, InstructionError>;

    /// Serializes `state` into the existing account data buffer.
    fn set_state(&self, state: &T) -> Result<(), InstructionError>;
}

/// Deserializes typed state from account data.
///
/// Invalid bytes map to `InstructionError::InvalidAccountData`.
fn state<T>(account: &impl crate::ReadableAccount) -> Result<T, InstructionError>
where
    T: serde::de::DeserializeOwned,
{
    crate::codec::deserialize_data(account).map_err(|_| InstructionError::InvalidAccountData)
}

/// Serializes typed state into an existing account buffer.
///
/// Oversized payloads map to `AccountDataTooSmall`; all other failures map to `GenericError`.
fn set_state<T>(
    account: &mut impl crate::WritableAccount,
    state: &T,
) -> Result<(), InstructionError>
where
    T: serde::Serialize,
{
    crate::codec::serialize_data(account, state).map_err(|err| match *err {
        ErrorKind::SizeLimit => InstructionError::AccountDataTooSmall,
        _ => InstructionError::GenericError,
    })
}

impl<T> StateMut<T> for Account
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    fn state(&self) -> Result<T, InstructionError> {
        state(self)
    }
    fn set_state(&mut self, state: &T) -> Result<(), InstructionError> {
        set_state(self, state)
    }
}

impl<T> StateMut<T> for AccountSharedData
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    fn state(&self) -> Result<T, InstructionError> {
        state(self)
    }
    fn set_state(&mut self, state: &T) -> Result<(), InstructionError> {
        set_state(self, state)
    }
}

impl<T> StateMut<T> for Ref<'_, AccountSharedData>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    fn state(&self) -> Result<T, InstructionError> {
        state(&**self)
    }
    fn set_state(&mut self, _state: &T) -> Result<(), InstructionError> {
        Err(InstructionError::ReadonlyDataModified)
    }
}
