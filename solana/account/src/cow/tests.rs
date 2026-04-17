use super::borrowed::BorrowedAccount;
use super::{StorageUnit, init, serialize_buf};
use crate::AccountBuilder;
use solana_pubkey::Pubkey;
use std::sync::atomic::Ordering::Acquire;

const BORROWED_LAMPORTS: u64 = 5;
const ACTIVE_DATA: &[u8] = &[1, 2, 3];
const COMMIT_DATA: &[u8] = &[4, 5, 6];
const COMMITTED_DATA: &[u8] = &[9, 2, 3];
const ACTIVE_WRITE: u8 = 9;
const ROLLBACK_WRITE: u8 = 8;
const INITIAL_SEQUENCE: u32 = 0;
const COMMITTED_SEQUENCE: u32 = 1;

// Serializes an owned account into a borrowed buffer image.
fn make_buf(data: &[u8]) -> Vec<StorageUnit> {
    let owner = Pubkey::new_unique();
    let owned = AccountBuilder::default()
        .lamports(BORROWED_LAMPORTS)
        .data(data.to_vec())
        .owner(owner)
        .build();
    serialize_buf(&owned)
}

// Reads the active sequence counter.
fn seq(acc: &BorrowedAccount) -> u32 {
    // SAFETY: test helpers only call this on a live borrowed buffer.
    unsafe { acc.header.as_ref().sequence.load(Acquire) }
}

// Returns the active image bytes for direct assertions.
fn data(acc: &BorrowedAccount) -> &[u8] {
    &acc.data
}

#[test]
// `init` should read the active image without changing the sequence.
fn test_init_reads_active_image() {
    let mut buf = make_buf(ACTIVE_DATA);
    let borrowed = init(&mut buf);

    assert_eq!(data(&borrowed), ACTIVE_DATA);
    assert_eq!(seq(&borrowed), INITIAL_SEQUENCE);
}

#[test]
// `translate` should copy the active image into the shadow view, and `commit` should publish it.
fn test_translate_commit_publishes_shadow_image() {
    let mut buf = make_buf(ACTIVE_DATA);
    let mut borrowed = init(&mut buf);

    // SAFETY: `borrowed` still points at the live borrowed image selected by `init`.
    unsafe { borrowed.translate() };
    assert_eq!(seq(&borrowed), INITIAL_SEQUENCE);

    borrowed.data[0] = ACTIVE_WRITE;
    borrowed.commit();
    assert_eq!(seq(&borrowed), COMMITTED_SEQUENCE);

    let borrowed = init(&mut buf);
    assert_eq!(data(&borrowed), COMMITTED_DATA);
}

#[test]
// `rollback` should discard shadow writes and restore the active view.
fn test_translate_rollback_discards_shadow_writes() {
    let mut buf = make_buf(COMMIT_DATA);
    let mut borrowed = init(&mut buf);

    // SAFETY: `borrowed` still points at the live borrowed image selected by `init`.
    unsafe { borrowed.translate() };
    borrowed.data[0] = ROLLBACK_WRITE;

    // SAFETY: `reset` is paired with the preceding `translate`.
    unsafe { borrowed.reset() };
    assert_eq!(seq(&borrowed), INITIAL_SEQUENCE);
    assert_eq!(data(&borrowed), COMMIT_DATA);

    let borrowed = init(&mut buf);
    assert_eq!(data(&borrowed), COMMIT_DATA);
}
