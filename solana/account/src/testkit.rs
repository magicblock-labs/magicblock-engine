#![allow(clippy::expect_used)]

use {
    crate::{
        AccountBuilder, AccountSharedData, BorrowedAccount, OwnedAccount, ReadableAccount,
        StorageUnit,
    },
    solana_pubkey::Pubkey,
    std::ptr::NonNull,
};

/// Builds a borrowed account image backed by serialized owned state.
pub fn borrowed_account_buffer(data: Vec<u8>, owner: Pubkey) -> Vec<StorageUnit> {
    let pubkey = Pubkey::new_unique();
    let owned = AccountBuilder::default()
        .lamports(1)
        .data(data)
        .owner(owner)
        .build::<OwnedAccount>();
    serialize_account_buffer(&owned, &pubkey)
}

/// Serializes an owned account into the borrowed layout used by tests.
pub fn serialize_account_buffer(owned: &OwnedAccount, pubkey: &Pubkey) -> Vec<StorageUnit> {
    let mut buf = vec![Default::default(); owned.units() as usize];
    // SAFETY: `buf` is sized from `units` and allocated with the alignment
    // required by the borrowed account layout.
    unsafe { owned.serialize(&mut buf, pubkey) };
    buf
}

/// Builds a borrowed account view over a test buffer.
pub fn init_borrowed_account(buf: &mut [StorageUnit]) -> BorrowedAccount {
    let ptr = NonNull::from(&mut buf[..]).cast();
    // SAFETY: test buffers are created with the borrowed account layout.
    unsafe { BorrowedAccount::init(ptr) }
}

/// Wraps a borrowed account test buffer in `AccountSharedData`.
pub fn borrowed_shared_data(buf: &mut [StorageUnit]) -> AccountSharedData {
    AccountSharedData::from(init_borrowed_account(buf))
}

/// Reinitializes a borrowed test buffer and returns its active data image.
pub fn active_borrowed_data(buf: &mut [StorageUnit]) -> Vec<u8> {
    borrowed_shared_data(buf).data().to_vec()
}
