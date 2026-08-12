#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = include_str!("../README.md")]

mod account;
#[cfg(feature = "bincode")]
mod codec;
mod cow;
mod patch;
#[cfg(feature = "bincode")]
pub mod state_traits;
#[cfg(feature = "bincode")]
mod sysvar;
/// Test-only helpers for borrowed account buffers.
#[cfg(feature = "testkit")]
pub mod testkit;
mod traits;

pub use account::{Account, PROGRAM_OWNERS, create_is_signer_account_infos};
pub use cow::{
    ALIGNMENT, AccountBuilder, AccountMode, AccountSeqLock, AccountSharedData, BorrowedAccount,
    CoWAccount, DirtyMarkers, OwnedAccount, STORAGE_UNIT, StateFlags, StorageUnit,
};
pub use patch::{AccountFieldPatch, AccountPatchError};
#[cfg(feature = "bincode")]
pub use sysvar::{
    DUMMY_INHERITABLE_ACCOUNT_FIELDS, InheritableAccountFields, create_account_for_test,
    create_account_shared_data_for_test, create_account_shared_data_with_fields,
    create_account_with_fields, from_account, to_account,
};
pub use traits::{ReadableAccount, WritableAccount, accounts_equal};

#[cfg(test)]
mod tests;
