use crate::{
    Account, AccountBuilder, AccountFieldPatch, AccountMode, AccountPatchError, AccountSeqLock,
    AccountSharedData, BorrowedAccount, CoWAccount, DirtyMarkers, OwnedAccount, ReadableAccount,
    StorageUnit, WritableAccount, accounts_equal,
    testkit::{delegated_account, init_borrowed_account, serialize_account_buffer},
};
use bincode::ErrorKind;
use solana_clock::Epoch;
use solana_instruction_error::LamportsError;
use solana_pubkey::Pubkey;
use std::{
    cell::{Cell, RefCell},
    ptr::NonNull,
};

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

/// Proves lifecycle patches preserve state on errors and mark accepted changes.
#[test]
fn test_account_patch_transition_errors() {
    use AccountMode as Mode;

    let mut account = delegated_account(0, vec![], Pubkey::default())
        .slot(10)
        .build::<AccountSharedData>();

    assert_eq!(
        account.set_lifecycle(Mode::Mirrored, 10),
        Err(AccountPatchError::InvalidModeTransition {
            from: Mode::Delegated,
            to: Mode::Mirrored,
        })
    );
    assert!(account.is(Mode::Delegated));
    assert!(account.markers().is_empty());

    assert_eq!(
        AccountFieldPatch::Lifecycle { mode: Mode::Delegated, slot: 10 }.apply(&mut account),
        Err(AccountPatchError::InvalidModeTransition {
            from: Mode::Delegated,
            to: Mode::Delegated,
        })
    );
    assert_eq!(account.slot(), 10);
    assert!(account.markers().is_empty());

    AccountFieldPatch::Lifecycle {
        mode: Mode::Undelegating,
        slot: 10,
    }
    .apply(&mut account)
    .unwrap();
    assert!(account.markers().contains(DirtyMarkers::MODE | DirtyMarkers::SLOT));

    let markers = *account.markers();
    assert_eq!(
        AccountFieldPatch::Lifecycle { mode: Mode::Mirrored, slot: 9 }.apply(&mut account),
        Err(AccountPatchError::InvalidSlotTransition { from: 10, to: 9 })
    );
    assert_eq!(account.slot(), 10);
    assert_eq!(*account.markers(), markers);

    let mut magic = AccountBuilder::default().mode(Mode::Magic).build::<AccountSharedData>();
    magic.set_lifecycle(Mode::Closed, 0).unwrap();
    assert!(magic.is(Mode::Closed));
}

/// Proves every lifecycle pair enforces slot ordering identically for owned and
/// borrowed accounts, preserving rejected state and marking only accepted changes.
/// Publishing a borrowed transition preserves the complete account image.
#[test]
fn test_lifecycle_matrix() {
    use AccountMode as Mode;

    // Columns are destinations in `modes` order.
    // 0 forbids the pair, 1 permits equal/newer slots, 2 requires a newer slot.
    let cases = [
        (Mode::Missing, [2, 1, 1, 1, 1, 0, 1]),
        (Mode::Mirrored, [2, 2, 0, 1, 1, 0, 1]),
        (Mode::Runtime, [0, 0, 2, 0, 0, 0, 0]),
        (Mode::Delegated, [0, 0, 0, 0, 0, 1, 0]),
        (Mode::Magic, [0, 0, 0, 1, 0, 0, 1]),
        (Mode::Undelegating, [1, 1, 0, 2, 0, 0, 1]),
        (Mode::Closed, [0, 0, 0, 0, 0, 0, 0]),
    ];
    let modes = cases.map(|(mode, _)| mode);
    let slot_pairs =
        [(10, 9), (10, 10), (10, 11), (0, 0), (0, 1), (u64::MAX, 0), (u64::MAX, u64::MAX)];
    let owner = Pubkey::new_unique();
    let key = Pubkey::new_unique();
    for (from, row) in cases {
        assert_eq!(row.len(), modes.len());
        for (to, rule) in modes.into_iter().zip(row) {
            for (from_slot, to_slot) in slot_pairs {
                let expected_result = if rule == 0 {
                    Err(AccountPatchError::InvalidModeTransition { from, to })
                } else if to_slot < from_slot || (rule == 2 && to_slot == from_slot) {
                    Err(AccountPatchError::InvalidSlotTransition { from: from_slot, to: to_slot })
                } else {
                    Ok(())
                };
                assert_eq!(
                    from.validate_transition(to, from_slot, to_slot),
                    expected_result,
                    "{from:?}@{from_slot} -> {to:?}@{to_slot}"
                );

                let original = AccountBuilder::default()
                    .mode(from)
                    .slot(from_slot)
                    .owner(owner)
                    .lamports(5)
                    .data(vec![1, 2, 3])
                    .executable(true)
                    .build::<OwnedAccount>();
                let mut buffer = serialize_account_buffer(&original, &key);
                let borrowed = AccountSharedData::from(init_borrowed_account(&mut buffer));
                let mut expected_account = original.clone();
                let mut expected_markers = DirtyMarkers::empty();
                if expected_result.is_ok() {
                    expected_account.core.mode = to;
                    expected_account.core.slot = to_slot;
                    expected_markers.insert(DirtyMarkers::SLOT);
                    expected_markers.set(DirtyMarkers::MODE, from != to);
                }
                for (storage, mut account) in
                    [("owned", AccountSharedData::from(original)), ("borrowed", borrowed)]
                {
                    assert_eq!(
                        account.set_lifecycle(to, to_slot),
                        expected_result,
                        "{storage}: {from:?}@{from_slot} -> {to:?}@{to_slot}"
                    );
                    assert_eq!(
                        *account.markers(),
                        expected_markers,
                        "{storage}: {from:?}@{from_slot} -> {to:?}@{to_slot}"
                    );
                    // Equality covers the complete core and data, independently of storage.
                    assert!(
                        account == expected_account,
                        "{storage}: {from:?}@{from_slot} -> {to:?}@{to_slot}"
                    );
                    if let CoWAccount::Borrowed(borrowed) = account.cow() {
                        // Publishing and reopening must preserve mode, flags, and data together.
                        borrowed.commit();
                        let active = AccountSharedData::from(init_borrowed_account(&mut buffer));
                        assert!(
                            active == expected_account,
                            "published: {from:?}@{from_slot} -> {to:?}@{to_slot}"
                        );
                    }
                }
            }
        }
    }
}

/// Pins mode properties, memory and wire encodings, and core field offsets.
/// Unsupported and truncated mode encodings must be rejected.
#[test]
fn test_mode_layout_and_decoding() {
    use crate::cow::AccountCore;
    use crate::mode::StateAuthority::{Base, Er};
    use AccountMode as Mode;
    use std::mem::{align_of, offset_of, size_of};

    assert_eq!(size_of::<Mode>(), 2);
    assert_eq!(align_of::<Mode>(), 2);
    assert_eq!(size_of::<AccountCore>(), 56);
    assert_eq!(align_of::<AccountCore>(), 8);
    assert_eq!(offset_of!(AccountCore, lamports), 0);
    assert_eq!(offset_of!(AccountCore, owner), 8);
    assert_eq!(offset_of!(AccountCore, slot), 40);
    assert_eq!(offset_of!(AccountCore, mode), 48);
    assert_eq!(offset_of!(AccountCore, flags), 50);

    let cases = [
        (Mode::Missing, [0, 4], None),
        (Mode::Mirrored, [1, 4], Some(Base)),
        (Mode::Runtime, [2, 0], Some(Er)),
        (Mode::Delegated, [1, 9], Some(Er)),
        (Mode::Magic, [1, 1], Some(Er)),
        (Mode::Undelegating, [1, 8], Some(Er)),
        (Mode::Closed, [3, 0], None),
    ];
    for (mode, bytes, authority) in cases {
        // SAFETY: repr(u16) stores a two-byte discriminant initialized for every variant.
        let raw = unsafe { &*std::ptr::from_ref(&mode).cast::<[u8; 2]>() };
        assert_eq!(*raw, bytes);
        assert_eq!(<[u8; 2]>::from(mode), bytes);
        assert_eq!(Mode::try_from(bytes).unwrap(), mode);
        assert_eq!(bincode::serialize(&mode).unwrap(), bytes);
        assert_eq!(bincode::deserialize::<Mode>(&bytes).unwrap(), mode);
        #[cfg(feature = "wincode")]
        {
            assert_eq!(wincode::serialize(&mode).unwrap(), bytes);
            assert_eq!(wincode::deserialize::<Mode>(&bytes).unwrap(), mode);
        }
        let [kind, policy] = bytes;
        assert_eq!(mode.kind() as u8, kind);
        assert_eq!(mode.access() as u8, policy & 3);
        assert_eq!(mode.data_flow() as u8, policy >> 2);
        assert_eq!(mode.authority(), authority);
        assert_eq!(
            mode.is_writable(),
            mode == Mode::Delegated || mode == Mode::Magic
        );
        assert_eq!(
            mode.requires_persistence(),
            mode == Mode::Delegated || mode == Mode::Magic || mode == Mode::Undelegating
        );
    }
    assert_eq!(Mode::default(), Mode::Missing);

    for bytes in [[4, 0], [1, 0x10], [1, 2], [1, 3], [1, 12], [2, 1], [1, 0]] {
        assert!(Mode::try_from(bytes).is_err());
        assert!(bincode::deserialize::<Mode>(&bytes).is_err());
        #[cfg(feature = "wincode")]
        assert!(wincode::deserialize::<Mode>(&bytes).is_err());
    }
    assert!(bincode::deserialize::<Mode>(&[1]).is_err());
    #[cfg(feature = "wincode")]
    assert!(wincode::deserialize::<Mode>(&[1]).is_err());
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
// Owned clones share their data until a write copies the shared buffer.
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
    let len = shared.data().len();
    let extra = vec![9; shared.capacity() - len + 1];

    shared.extend_from_slice(&extra);

    assert_eq!(&shared.data()[..len], &[7, 8]);
    assert_eq!(&shared.data()[len..], extra.as_slice());
    assert!(matches!(shared.cow(), CoWAccount::Owned(_)));
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
    assert!(matches!(shared.cow(), CoWAccount::Borrowed(_)));

    // An overlapping write with a tail beyond capacity must promote once.
    let (_buf, mut shared) = make_borrowed(vec![1, 2, 3]);
    let offset = 2;
    let data = vec![7; shared.capacity() - offset + 1];
    shared.set_data_at(offset, &data);
    assert_eq!(&shared.data()[..offset], &[1, 2]);
    assert_eq!(&shared.data()[offset..], data.as_slice());
    assert!(matches!(shared.cow(), CoWAccount::Owned(_)));
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
    let ptr = NonNull::from(buf.as_mut_slice()).cast();
    // SAFETY: `ptr` names the live serialized buffer for the duration of both
    // views. Their intentionally aliased access is sequenced by the test: the
    // reader is idle while the writer publishes, then resets before retrying.
    let borrowed = unsafe { BorrowedAccount::init(ptr) };
    // SAFETY: same buffer and access protocol as the reader view above.
    let writer = RefCell::new(unsafe { BorrowedAccount::init(ptr) });
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
