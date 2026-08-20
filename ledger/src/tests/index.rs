//! Index unit tests.

use fjall::Keyspace;
use nucleus::{
    Slot,
    testkit::{TempDir, init_tracing, tempdir},
};
use solana_pubkey::Pubkey;
use solana_signature::Signature;

use crate::{
    index::{Index, IndexReader, Span, TxSpan},
    storage::Durability,
};

/// Opens a fresh index on a throwaway directory kept alive by the returned guard.
fn index() -> (TempDir, Index, Keyspace) {
    init_tracing();
    let dir = tempdir();
    let index = Index::new(dir.path()).unwrap();
    let keyspace = index.keyspace(1).unwrap();
    (dir, index, keyspace)
}

/// Drains every execution span the account index holds for `pubkey`.
fn account_spans(keyspace: &Keyspace, pubkey: &Pubkey) -> Vec<Span> {
    IndexReader::new(keyspace)
        .accounts(pubkey, None)
        .map(|entry| entry.unwrap())
        .collect()
}

#[test]
fn span_pack_and_order() {
    let span = Span::new(1234, 56);
    assert_eq!(span.offset(), 1234);
    assert_eq!(span.size(), 56);

    // Boundary values: full size field, and an offset filling every high bit.
    let max_size = Span::new(0, Span::MAX_SIZE);
    assert_eq!((max_size.offset(), max_size.size()), (0, Span::MAX_SIZE));
    let max_offset = u64::MAX >> 25;
    let high = Span::new(max_offset, 0);
    assert_eq!((high.offset(), high.size()), (max_offset, 0));

    // Ordering is offset-dominant: a later offset outranks any size at an
    // earlier offset, and size only breaks ties within one offset.
    assert!(Span::new(11, 0) > Span::new(10, Span::MAX_SIZE));
    assert!(Span::new(10, 7) > Span::new(10, 5));
}

#[test]
fn transaction_and_block_roundtrip() {
    let (_dir, index, keyspace) = index();
    let signature = Signature::from([7; 64]);
    let txspan = TxSpan {
        blockstore: Span::new(10, 20),
        execution: Span::new(30, 40),
    };
    let (slot_a, slot_b): (Slot, Slot) = (1, 2);
    let block_a = Span::new(0, 8);
    let block_b = Span::new(8, 16);

    let mut writer = index.writer(&keyspace);
    writer.insert_transaction(&signature, txspan);
    writer.insert_block(slot_a, block_a);
    writer.insert_block(slot_b, block_b);
    writer.persist(Durability::Buffer).unwrap();

    let reader = IndexReader::new(&keyspace);
    let got = reader.transaction(&signature).unwrap().expect("transaction present");
    assert_eq!(got.blockstore, txspan.blockstore);
    assert_eq!(got.execution, txspan.execution);
    assert_eq!(reader.block(slot_a).unwrap(), Some(block_a));
    assert_eq!(reader.block(slot_b).unwrap(), Some(block_b));
    assert_eq!(
        reader.blocks(slot_a..=slot_b).map(|entry| entry.unwrap().0).collect::<Vec<_>>(),
        vec![slot_b, slot_a],
        "big-endian slot keys scan newest first"
    );

    // Absent keys resolve to nothing rather than a stale or default hit.
    assert!(reader.transaction(&Signature::from([9; 64])).unwrap().is_none());
    assert_eq!(reader.block(99).unwrap(), None);
}

#[test]
fn account_signature_duplicates() {
    let (_dir, index, keyspace) = index();
    let account = Pubkey::new_unique();
    let other = Pubkey::new_unique();
    let spans = [Span::new(100, 10), Span::new(200, 20), Span::new(300, 30)];
    let other_span = Span::new(400, 40);

    let mut writer = index.writer(&keyspace);
    for span in &spans {
        writer.insert_accounts(&[account], *span);
    }
    writer.insert_accounts(&[other], other_span);
    writer.persist(Durability::Buffer).unwrap();

    // Account duplicate spans are newest-first, so later execution offsets are returned first.
    assert_eq!(
        account_spans(&keyspace, &account),
        vec![spans[2], spans[1], spans[0]]
    );

    // Duplicates stay partitioned per account key.
    assert_eq!(account_spans(&keyspace, &other), vec![other_span]);

    let reader = IndexReader::new(&keyspace);
    assert_eq!(
        reader
            .accounts(&account, Some(spans[2]))
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>(),
        vec![spans[1], spans[0]],
        "exclusive big-endian cutoff starts pagination below the requested span"
    );
}

/// Proves deleting one superblock keyspace preserves in-flight reads and isolates reuse.
#[test]
fn superblock_keyspace_deletion_is_lease_safe() {
    let (_dir, index, keyspace) = index();
    let span = Span::new(10, 20);
    let mut writer = index.writer(&keyspace);
    writer.insert_block(1, span);
    writer.persist(Durability::Buffer).unwrap();

    let reader = IndexReader::new(&keyspace);
    index.delete(keyspace.clone()).unwrap();
    assert_eq!(reader.block(1).unwrap(), Some(span));

    let replacement = index.keyspace(1).unwrap();
    assert_eq!(IndexReader::new(&replacement).block(1).unwrap(), None);
}
