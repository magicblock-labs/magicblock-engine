use crate::{
    Account, AccountBuilder, AccountFieldPatch, AccountMode, AccountPatchError, AccountSeqLock,
    AccountSharedData, CoWAccount, DirtyMarkers, OwnedAccount, ReadableAccount, StorageUnit,
    WritableAccount, accounts_equal,
    testkit::{init_borrowed_account, serialize_account_buffer},
};
use bincode::ErrorKind;
use solana_clock::Epoch;
use solana_instruction_error::LamportsError;
use solana_pubkey::Pubkey;
use std::cell::{Cell, RefCell};

// Builds matching owned and shared accounts for baseline assertions.
fn make_two_accounts() -> (Pubkey, Account, AccountSharedData) {
    let key = Pubkey::new_unique();
    let mut account = Account::new(1, 2, &key);
    account.executable = true;
    account.rent_epoch = Epoch::MAX;

    let mut shared = AccountSharedData::new(1, 2, &key);
    shared.set_executable(true);
    shared.set_rent_epoch(4);

    assert!(accounts_equal(&account, &shared));
    (key, account, shared)
}

// Builds a borrowed account image backed by serialized owned state.
fn make_borrowed(data: Vec<u8>) -> (Vec<StorageUnit>, AccountSharedData) {
    let pubkey = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let owned = AccountBuilder::default().lamports(5).data(data).owner(owner).build();
    let mut buf = serialize_account_buffer(&owned, &pubkey);
    let borrowed = init_borrowed_account(&mut buf);
    (buf, AccountSharedData::from(borrowed))
}

fn assert_add_err<T: WritableAccount>(mut account: T) {
    assert!(matches!(
        account.checked_add_lamports(u64::MAX),
        Err(LamportsError::ArithmeticOverflow)
    ));
}

fn assert_sub_err<T: WritableAccount>(mut account: T) {
    assert!(matches!(
        account.checked_sub_lamports(u64::MAX),
        Err(LamportsError::ArithmeticUnderflow)
    ));
}

fn assert_saturating_add<T: WritableAccount + ReadableAccount>(
    mut account: T,
    start: u64,
    add: u64,
    expected: u64,
) {
    account.set_lamports(start);
    account.saturating_add_lamports(add);
    assert_eq!(account.lamports(), expected);
}

fn assert_saturating_sub<T: WritableAccount + ReadableAccount>(
    mut account: T,
    start: u64,
    sub: u64,
    expected: u64,
) {
    account.set_lamports(start);
    account.saturating_sub_lamports(sub);
    assert_eq!(account.lamports(), expected);
}

#[test]
// Owner bytes should copy into both account representations identically.
fn test_account_data_copy_as_slice() {
    let key2 = Pubkey::new_unique();
    let (_, mut account1, mut account2) = make_two_accounts();
    account1.copy_into_owner_from_slice(key2.as_ref());
    account2.copy_into_owner_from_slice(key2.as_ref());
    assert!(accounts_equal(&account1, &account2));
    assert_eq!(account1.owner(), &key2);
}

#[test]
// set_data_from_slice should overwrite, grow, shrink, and preserve contents.
fn test_account_set_data_from_slice() {
    let (_, _, mut account) = make_two_accounts();
    assert_eq!(account.data(), &[0, 0]);
    account.set_data_from_slice(&[1, 2]);
    assert_eq!(account.data(), &[1, 2]);
    account.set_data_from_slice(&[1, 2, 3]);
    assert_eq!(account.data(), &[1, 2, 3]);
    account.set_data_from_slice(&[4, 5, 6]);
    assert_eq!(account.data(), &[4, 5, 6]);
    account.set_data_from_slice(&[4, 5, 6, 0]);
    assert_eq!(account.data(), &[4, 5, 6, 0]);
    account.set_data_from_slice(&[]);
    assert_eq!(account.data(), &[]);
    account.set_data_from_slice(&[44]);
    assert_eq!(account.data(), &[44]);
    account.set_data_from_slice(&[44]);
    assert_eq!(account.data(), &[44]);
}

#[test]
// set_data_at only writes and extends, so an empty write leaves the buffer as is.
fn test_account_set_data_at_never_truncates() {
    let (_, _, mut account) = make_two_accounts();
    assert_eq!(account.data(), &[0, 0]);
    account.set_data_at(0, &[1, 2]);
    assert_eq!(account.data(), &[1, 2]);
    account.set_data_at(0, &[]);
    assert_eq!(account.data(), &[1, 2]);
}

#[test]
// Data patches should write in place and extend when needed.
fn test_account_field_patch_data_at() {
    let owner = Pubkey::new_unique();
    let mut account = AccountSharedData::new(1, 2, &owner);
    account.set_data_from_slice(&[1, 2, 3, 4]);

    AccountFieldPatch::DataAt {
        offset: 1,
        data: vec![9, 8, 7, 6],
    }
    .apply(&mut account)
    .unwrap();
    assert_eq!(account.data(), &[1, 9, 8, 7, 6]);

    AccountFieldPatch::DataAt { offset: 6, data: vec![5, 4] }
        .apply(&mut account)
        .unwrap();
    assert_eq!(account.data(), &[1, 9, 8, 7, 6, 0, 5, 4]);
}

#[test]
fn test_account_patch_transition_errors() {
    let mut account = AccountBuilder::default()
        .mode(AccountMode::Delegated)
        .slot(10)
        .build::<AccountSharedData>();

    assert_eq!(
        account.set_mode(AccountMode::ReadOnly),
        Err(AccountPatchError::InvalidModeTransition {
            from: AccountMode::Delegated,
            to: AccountMode::ReadOnly,
        })
    );
    assert!(account.is(AccountMode::Delegated));
    assert!(account.markers().is_empty());

    assert_eq!(
        AccountFieldPatch::Slot(10).apply(&mut account),
        Err(AccountPatchError::InvalidSlotTransition { from: 10, to: 10 })
    );
    assert_eq!(account.slot(), 10);
    assert!(account.markers().is_empty());

    account.set_mode(AccountMode::Transient).unwrap();
    AccountFieldPatch::Slot(10).apply(&mut account).unwrap();
    assert!(account.markers().contains(DirtyMarkers::MODE));
    assert!(account.markers().contains(DirtyMarkers::SLOT));

    let markers = *account.markers();
    assert_eq!(
        AccountFieldPatch::Slot(9).apply(&mut account),
        Err(AccountPatchError::InvalidSlotTransition { from: 10, to: 9 })
    );
    assert_eq!(account.slot(), 10);
    assert_eq!(*account.markers(), markers);

    let mut ephemeral = AccountBuilder::default()
        .mode(AccountMode::Ephemeral)
        .build::<AccountSharedData>();
    ephemeral.set_mode(AccountMode::Closed).unwrap();
    assert!(ephemeral.is(AccountMode::Closed));
}

#[test]
// Deserialization should fail on a non-bincode payload.
fn test_account_deserialize() {
    let (_, account1, _) = make_two_accounts();
    assert!(account1.deserialize_data::<String>().is_err());
}

#[test]
// Serialization should reject values larger than the data buffer.
fn test_account_serialize() {
    let (_, mut account1, _) = make_two_accounts();
    let err = account1.serialize_data(&"hello world").unwrap_err();
    assert!(matches!(*err, ErrorKind::SizeLimit));
}

#[test]
// Shared accounts should fail deserialization on the same invalid payload.
fn test_account_cow_deserialize() {
    let (_, _, account2) = make_two_accounts();
    assert!(account2.deserialize_data::<String>().is_err());
}

#[test]
// Shared accounts should reject oversized serialization too.
fn test_account_cow_serialize() {
    let (_, _, mut account2) = make_two_accounts();
    let err = account2.serialize_data(&"hello world").unwrap_err();
    assert!(matches!(*err, ErrorKind::SizeLimit));
}

#[test]
// Account and AccountSharedData should expose the same visible state.
fn test_account_cow() {
    let (key, account1, account2) = make_two_accounts();
    assert!(accounts_equal(&account1, &account2));

    assert_eq!(account1.lamports, 1);
    assert_eq!(account1.lamports(), 1);
    assert_eq!(account1.data.len(), 2);
    assert_eq!(account1.data().len(), 2);
    assert_eq!(account1.owner, key);
    assert_eq!(account1.owner(), &key);
    assert!(account1.executable);
    assert!(account1.executable());
    assert_eq!(account1.rent_epoch, Epoch::MAX);
    assert_eq!(account1.rent_epoch(), Epoch::MAX);

    assert_eq!(account2.lamports(), 1);
    assert_eq!(account2.data().len(), 2);
    assert_eq!(account2.owner(), &key);
    assert!(account2.executable());
    assert_eq!(account2.rent_epoch(), Epoch::MAX);
}

#[test]
// Checked lamport mutation should keep both account forms in sync.
fn test_account_add_sub_lamports() {
    let (_, mut account1, mut account2) = make_two_accounts();
    assert!(accounts_equal(&account1, &account2));
    assert!(matches!(account1.checked_add_lamports(1), Ok(())));
    assert!(matches!(account2.checked_add_lamports(1), Ok(())));
    assert!(accounts_equal(&account1, &account2));
    assert_eq!(account1.lamports(), 2);
    assert!(matches!(account1.checked_sub_lamports(2), Ok(())));
    assert!(matches!(account2.checked_sub_lamports(2), Ok(())));
    assert!(accounts_equal(&account1, &account2));
    assert_eq!(account1.lamports(), 0);
}

#[test]
// Checked lamport arithmetic should report overflow and underflow.
fn test_account_checked_lamport_errors() {
    let (_, account1, account2) = make_two_accounts();

    assert_add_err(account1.clone());
    assert_sub_err(account1);
    assert_add_err(account2.clone());
    assert_sub_err(account2);
}

#[test]
// Saturating lamport arithmetic should clamp on both account forms.
fn test_account_saturating_lamports() {
    let (_, account1, account2) = make_two_accounts();

    assert_saturating_add(account1.clone(), u64::MAX - 22, 44, u64::MAX);
    assert_saturating_add(account2.clone(), u64::MAX - 22, 44, u64::MAX);
    assert_saturating_sub(account1, 33, 66, 0);
    assert_saturating_sub(account2, 33, 66, 0);
}

#[test]
// Shrinking data should replace the contents and allow regrowth.
fn test_account_cow_set_data_from_slice_shrinks() {
    let owner = Pubkey::new_unique();
    let mut shared = AccountSharedData::new(1, 4, &owner);

    shared.set_data_from_slice(&[1, 2, 3, 4]);
    assert_eq!(shared.data(), &[1, 2, 3, 4]);

    shared.set_data_from_slice(&[]);
    assert_eq!(shared.data(), &[]);

    shared.set_data_from_slice(&[9]);
    assert_eq!(shared.data(), &[9]);
}

#[test]
// Cloning should share storage until a write forces promotion.
fn test_account_cow_is_copy_on_write() {
    let owner = Pubkey::new_unique();
    let mut shared = AccountSharedData::new(1, 2, &owner);
    shared.set_data_from_slice(&[1, 2]);

    let cloned = shared.clone();
    assert!(shared.is_shared());
    assert!(cloned.is_shared());

    shared.extend_from_slice(&[3]);
    assert_eq!(shared.data(), &[1, 2, 3]);
    assert_eq!(cloned.data(), &[1, 2]);
}

#[test]
// Borrowed serialization should round-trip back to shared state.
fn test_account_cow_borrowed_round_trip() {
    let pubkey = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let owned = AccountBuilder::default()
        .lamports(5)
        .data(vec![7, 8])
        .owner(owner)
        .executable(true)
        .build::<OwnedAccount>();
    let expected: AccountSharedData = owned.clone().into();
    let mut buf = serialize_account_buffer(&owned, &pubkey);
    let borrowed = init_borrowed_account(&mut buf);
    let shared = AccountSharedData::from(borrowed);

    assert_eq!(shared, expected);
}

#[test]
// Writing past borrowed capacity should promote to owned storage.
fn test_account_cow_borrowed_extend_promotes() {
    let (_buf, mut shared) = make_borrowed(vec![7, 8]);

    shared.extend_from_slice(&[9]);

    assert_eq!(shared.data(), &[7, 8, 9]);
}

#[test]
// Exact-capacity borrowed writes should keep the existing bytes intact.
fn test_account_cow_borrowed_exact_capacity_writes() {
    let (_buf, mut shared) = make_borrowed(vec![1, 2]);
    let snap = shared.data_clone();
    let cap = shared.capacity();

    assert!(cap > shared.data().len());

    shared.resize(cap, 0x55);
    assert_eq!(shared.data().len(), cap);
    assert_eq!(&shared.data()[..2], &[1, 2]);
    assert!(shared.data()[2..].iter().all(|&b| b == 0x55));

    let repl = vec![0x9a; cap];
    shared.set_data_from_slice(&repl);
    assert_eq!(shared.data(), repl.as_slice());
    assert_eq!(snap.as_ref(), &[1, 2]);
}

#[test]
// Borrowed resize must write the shadow image before commit publishes it.
fn test_account_cow_borrowed_resize_survives_commit() {
    let (mut buf, mut shared) = make_borrowed(vec![1, 2]);
    let cap = shared.capacity();

    shared.resize(cap, 0x55);
    let CoWAccount::Borrowed(borrowed) = shared.cow() else {
        panic!("resize within borrowed capacity should not promote");
    };
    borrowed.commit();
    drop(shared);

    let borrowed = init_borrowed_account(&mut buf);
    assert_eq!(borrowed.data.len(), cap);
    assert_eq!(&borrowed.data[..2], &[1, 2]);
    assert!(borrowed.data[2..].iter().all(|&b| b == 0x55));
}

#[test]
// Overflowing borrowed writes should preserve existing bytes through promotion.
fn test_account_cow_borrowed_overflow_promotes_without_corruption() {
    let (_buf, mut shared) = make_borrowed(vec![3, 4]);
    let snap = shared.data_clone();
    let cap = shared.capacity();
    let extra = vec![0xab; cap - shared.data().len() + 1];
    let mut exp = vec![3, 4];
    exp.extend_from_slice(&extra);

    shared.extend_from_slice(&extra);

    assert_eq!(shared.data(), exp.as_slice());
    assert_eq!(snap.as_ref(), &[3, 4]);
}

#[test]
fn test_cow_set_data_at_borrowed_promotes_once() {
    // In-place overlap: `offset < len` writes entirely through `data_as_mut_slice`.
    let (_buf, mut shared) = make_borrowed(vec![1, 2, 3, 4]);
    shared.set_data_at(1, &[9, 9]);
    assert_eq!(shared.data(), &[1, 9, 9, 4]);

    // In-place head write that then grows past `len` via `extend_from_slice`.
    let (_buf, mut shared) = make_borrowed(vec![1, 2, 3]);
    shared.set_data_at(2, &[7, 8, 9]);
    assert_eq!(shared.data(), &[1, 2, 7, 8, 9]);
}

#[test]
// `init` should read the active image without changing the sequence.
fn test_cow_init_reads_active_image() {
    let pubkey = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let owned = AccountBuilder::default().lamports(5).data(vec![1, 2, 3]).owner(owner).build();
    let mut buf = serialize_account_buffer(&owned, &pubkey);
    let borrowed = init_borrowed_account(&mut buf);

    assert_eq!(&*borrowed.data, &[1, 2, 3]);
    assert_eq!(borrowed.sequence(), 0);
}

#[test]
// `translate` should copy the active image into the shadow view, and `commit` should publish it.
fn test_cow_translate_commit_publishes_shadow_image() {
    let pubkey = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let owned = AccountBuilder::default().lamports(5).data(vec![1, 2, 3]).owner(owner).build();
    let mut buf = serialize_account_buffer(&owned, &pubkey);
    let mut borrowed = init_borrowed_account(&mut buf);

    // SAFETY: `borrowed` still points at the live borrowed image selected by `init`.
    unsafe { borrowed.translate() };
    assert_eq!(borrowed.sequence(), 0);

    borrowed.data[0] = 9;
    borrowed.commit();
    assert_eq!(borrowed.sequence(), 1);

    let borrowed = init_borrowed_account(&mut buf);
    assert_eq!(&*borrowed.data, &[9, 2, 3]);
}

#[test]
// Reset should discard shadow writes and re-read the active image.
fn test_cow_translate_rollback_discards_shadow_writes() {
    let pubkey = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let owned = AccountBuilder::default().lamports(5).data(vec![4, 5, 6]).owner(owner).build();
    let mut buf = serialize_account_buffer(&owned, &pubkey);
    let mut borrowed = init_borrowed_account(&mut buf);

    // SAFETY: `borrowed` still points at the live borrowed image selected by `init`.
    unsafe { borrowed.translate() };
    borrowed.data[0] = 8;

    // SAFETY: `reset` only repoints this view back to the active image.
    unsafe { borrowed.reset() };
    assert_eq!(borrowed.sequence(), 0);
    assert_eq!(&*borrowed.data, &[4, 5, 6]);

    let borrowed = init_borrowed_account(&mut buf);
    assert_eq!(&*borrowed.data, &[4, 5, 6]);
}

#[test]
// AccountSeqLock should retry against the newly published borrowed image.
fn test_account_seq_lock_read_retries_after_borrowed_publish() {
    let pubkey = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let owned = AccountBuilder::default().lamports(5).data(vec![1, 2, 3]).owner(owner).build();
    let mut buf = serialize_account_buffer(&owned, &pubkey);
    let borrowed = init_borrowed_account(&mut buf);
    let writer = RefCell::new(init_borrowed_account(&mut buf));
    let mut lock = AccountSeqLock::new(AccountSharedData::from(borrowed));
    let calls = Cell::new(0);

    let data = lock.read(|account| {
        let call = calls.get();
        calls.set(call + 1);

        if call == 0 {
            let mut writer = writer.borrow_mut();
            // SAFETY: the writer still points at the image selected by `init`.
            unsafe { writer.translate() };
            writer.data[0] = 9;
            writer.commit();
        }

        account.data().to_vec()
    });

    assert_eq!(calls.get(), 2);
    assert_eq!(data, vec![9, 2, 3]);
}
