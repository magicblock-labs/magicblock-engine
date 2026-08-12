use {
    crate::{Account, AccountSharedData, ReadableAccount, WritableAccount},
    solana_clock::{Epoch, INITIAL_RENT_EPOCH},
    solana_sysvar::SysvarSerialize,
};

/// Fields copied into test sysvar accounts.
pub type InheritableAccountFields = (u64, Epoch);

/// Default lamports and rent epoch used by test sysvar account helpers.
pub const DUMMY_INHERITABLE_ACCOUNT_FIELDS: InheritableAccountFields = (1, INITIAL_RENT_EPOCH);

/// Serializes a sysvar into account data and pads it to the declared size.
///
/// Serialization failure falls back to zeroed bytes sized to `S::size_of()`.
/// That keeps the helper infallible while preserving the advertised layout.
fn account_data<S: SysvarSerialize>(sysvar: &S) -> Vec<u8> {
    let mut data = bincode::serialize(sysvar).unwrap_or_default();
    data.resize(data.len().max(S::size_of()), 0);
    data
}

/// Creates an [`Account`] that contains a serialized sysvar value.
pub fn create_account_with_fields<S: SysvarSerialize>(
    sysvar: &S,
    (lamports, rent_epoch): InheritableAccountFields,
) -> Account {
    Account {
        lamports,
        data: account_data(sysvar),
        owner: solana_sdk_ids::sysvar::id(),
        executable: false,
        rent_epoch,
    }
}

/// Creates a test sysvar [`Account`].
pub fn create_account_for_test<S: SysvarSerialize>(sysvar: &S) -> Account {
    create_account_with_fields(sysvar, DUMMY_INHERITABLE_ACCOUNT_FIELDS)
}

/// Creates an [`AccountSharedData`] that contains a serialized sysvar value.
pub fn create_account_shared_data_with_fields<S: SysvarSerialize>(
    sysvar: &S,
    fields: InheritableAccountFields,
) -> AccountSharedData {
    AccountSharedData::from(create_account_with_fields(sysvar, fields))
}

/// Creates a test sysvar [`AccountSharedData`].
pub fn create_account_shared_data_for_test<S: SysvarSerialize>(sysvar: &S) -> AccountSharedData {
    create_account_shared_data_with_fields(sysvar, DUMMY_INHERITABLE_ACCOUNT_FIELDS)
}

/// Deserializes a sysvar value from account data.
///
/// Returns `None` on decode failure.
pub fn from_account<S: SysvarSerialize, T: ReadableAccount>(account: &T) -> Option<S> {
    bincode::deserialize(account.data()).ok()
}

/// Serializes a sysvar value into account data.
///
/// Returns `None` on encode failure.
pub fn to_account<S: SysvarSerialize, T: WritableAccount>(
    sysvar: &S,
    account: &mut T,
) -> Option<()> {
    bincode::serialize_into(account.data_as_mut_slice(), sysvar).ok()
}
