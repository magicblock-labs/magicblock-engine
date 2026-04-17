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
mod traits;

pub use account::{create_is_signer_account_infos, Account, PROGRAM_OWNERS};
pub use cow::{
    AccountBuilder, AccountMode, AccountSharedData, BorrowedAccount, CoWAccount, DirtyMarkers,
    OwnedAccount, StateFlags, StorageUnit, ALIGNMENT, STORAGE_UNIT,
};
pub use patch::AccountFieldPatch;
#[cfg(feature = "bincode")]
pub use sysvar::{
    create_account_for_test, create_account_shared_data_for_test,
    create_account_shared_data_with_fields, create_account_with_fields, from_account, to_account,
    InheritableAccountFields, DUMMY_INHERITABLE_ACCOUNT_FIELDS,
};
pub use traits::{accounts_equal, ReadableAccount, WritableAccount};

#[cfg(test)]
mod tests;
