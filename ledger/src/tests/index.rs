//! Index unit tests.

use heed::RwTxn;
use nucleus::{
    Slot,
    testkit::{TempDir, init_tracing, tempdir},
};
use solana_pubkey::Pubkey;
use solana_signature::Signature;

use crate::index::{Index, Span, TxSpan};

/// Opens a fresh index on a throwaway directory kept alive by the returned guard.
fn index() -> (TempDir, Index) {
    init_tracing();
    let dir = tempdir();
    let index = Index::new(dir.path()).unwrap();
    (dir, index)
}

/// Commits a lazily opened write transaction, if any inserts opened one.
fn commit(txn: Option<RwTxn<'_>>) {
    if let Some(txn) = txn {
        txn.commit().unwrap();
    }
}

/// Drains every execution span the account index holds for `pubkey`.
fn account_spans(index: &Index, pubkey: &Pubkey) -> Vec<Span> {
    let mut txn = None;
    let Some(iter) = index.accounts(pubkey, &mut txn).unwrap() else {
        return Vec::new();
    };
    iter.map(|entry| entry.unwrap().1).collect()
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
    let (_dir, index) = index();
    let signature = Signature::from([7; 64]);
    let txspan = TxSpan {
        blockstore: Span::new(10, 20),
        execution: Span::new(30, 40),
    };
    let (slot_a, slot_b): (Slot, Slot) = (1, 2);
    let block_a = Span::new(0, 8);
    let block_b = Span::new(8, 16);

    let mut txn = None;
    index.insert_transaction(&mut txn, &signature, &txspan).unwrap();
    index.insert_block(&mut txn, &slot_a, &block_a).unwrap();
    index.insert_block(&mut txn, &slot_b, &block_b).unwrap();
    commit(txn);

    let mut txn = None;
    let got = index.transaction(&signature, &mut txn).unwrap().expect("transaction present");
    assert_eq!(got.blockstore, txspan.blockstore);
    assert_eq!(got.execution, txspan.execution);
    assert_eq!(index.block(&slot_a, &mut txn).unwrap(), Some(block_a));
    assert_eq!(index.block(&slot_b, &mut txn).unwrap(), Some(block_b));

    // Absent keys resolve to nothing rather than a stale or default hit.
    assert!(index.transaction(&Signature::from([9; 64]), &mut txn).unwrap().is_none());
    assert_eq!(index.block(&99, &mut txn).unwrap(), None);
}

#[test]
fn account_signature_duplicates() {
    let (_dir, index) = index();
    let account = Pubkey::new_unique();
    let other = Pubkey::new_unique();
    let spans = [Span::new(100, 10), Span::new(200, 20), Span::new(300, 30)];
    let other_span = Span::new(400, 40);

    let mut txn = None;
    for span in &spans {
        index.insert_accounts(&mut txn, &[account], span).unwrap();
    }
    index.insert_accounts(&mut txn, &[other], &other_span).unwrap();
    commit(txn);

    // Account duplicate spans are newest-first, so later execution offsets are returned first.
    assert_eq!(
        account_spans(&index, &account),
        vec![spans[2], spans[1], spans[0]]
    );

    // Duplicates stay partitioned per account key.
    assert_eq!(account_spans(&index, &other), vec![other_span]);
}
