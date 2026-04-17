//! Shared bincode helpers for account data.

use serde::{Serialize, de::DeserializeOwned};

use crate::{ReadableAccount, WritableAccount};

/// Deserializes typed state from an account data slice.
pub(crate) fn deserialize_data<T: DeserializeOwned, U: ReadableAccount>(
    account: &U,
) -> Result<T, bincode::Error> {
    bincode::deserialize(account.data())
}

/// Serializes typed state into an existing account data buffer.
pub(crate) fn serialize_data<T: Serialize, U: WritableAccount>(
    account: &mut U,
    state: &T,
) -> Result<(), bincode::Error> {
    if bincode::serialized_size(state)? > account.data().len() as u64 {
        return Err(Box::new(bincode::ErrorKind::SizeLimit));
    }
    bincode::serialize_into(account.data_as_mut_slice(), state)
}
