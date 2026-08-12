//! End-to-end tests over the append→seal→read pipeline.
//!
//! Each test drives real [`Event`]s through the [`LedgerAppender`] and reads
//! them back through the [`LedgerReader`] without starting the service pool.
//! Single-response readers run synchronously on the test thread; replay uses a
//! worker thread so the test can drain its bounded channel concurrently.
//! Transactions are genuine wincode-serialized Solana transactions so the
//! appender's account/signature extraction and the reader's block reconstruction
//! exercise the real codecs.
//!
//! The appender only makes data durable at a block boundary (sync + index
//! commit + cursor publish), so every append batch here ends with a `Block`;
//! that also mirrors how a caller must frame writes.

use std::{
    ops::Range,
    sync::{Arc, atomic::Ordering::Acquire},
};

use nucleus::{
    MB, Slot,
    ledger::{Block, SuperblockSeal},
    shutdown::{Service, ShutdownManager},
    testkit::{TempDir, init_tracing, tempdir, transaction},
};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction_error::TransactionResult;
use tokio::sync::{broadcast, mpsc};

use crate::{
    Ledger,
    appender::LedgerAppender,
    reader::LedgerReader,
    request::{
        AccountSignature, AccountSignaturesParams, BlockDetails, BlockParams, BlockResponse,
        ReadRequest, ReplayParams, RequestPayload, TransactionResponse,
    },
    schema::{
        Balances, Event, Execution, ExecutionDetails, ExecutionHeader, OwnedBlockstoreEntry,
        TransactionEntry,
    },
};

/// Fresh ledger on a throwaway directory; the `TempDir` must outlive the ledger.
///
/// `size_limit` gates retention: `u64::MAX` disables it, `0` forces `truncate`
/// to run at every block boundary (the whole ledger filesystem always counts as
/// "over budget").
fn ledger(size_limit: u64) -> (TempDir, Arc<Ledger>) {
    init_tracing();
    let dir = tempdir();
    let ledger = Arc::new(Ledger::new(dir.path().to_owned(), size_limit).unwrap());
    (dir, ledger)
}

/// Feeds `events` through a freshly opened appender and runs it to completion.
///
/// The appender resumes from on-disk cursors, so successive calls model both
/// continued appends and a restart of the write side.
fn append(ledger: &Arc<Ledger>, events: Vec<Event>) {
    let (tx, rx) = flume::bounded(events.len().max(1));
    let (position, _) = broadcast::channel(64);
    for event in events {
        tx.send(event).unwrap();
    }
    drop(tx);
    let mut shutdown = ShutdownManager::default();
    LedgerAppender::new(ledger.clone(), rx, position)
        .unwrap()
        .run(shutdown.handle(Service::LedgerAppender));
}

/// Execution metadata carrying a recognizable `fee`/`logs` for read assertions.
fn execution(signature: Signature, slot: Slot, result: TransactionResult<()>) -> Execution {
    Execution {
        header: ExecutionHeader { signature, result, slot },
        details: Some(ExecutionDetails {
            fee: slot * 1000,
            balances: Balances { pre: vec![1], post: vec![2] },
            logs: Arc::new(vec![format!("log for {slot}")]),
            cpi: None,
            compute_units: slot * 7,
            return_data: None,
        }),
    }
}

/// The `Transaction` + paired `Execution` events that record one transaction in
/// a block at `slot` (execution result `Ok`).
fn recorded(sig: Signature, payload: Arc<Vec<u8>>, slot: Slot) -> [Event; 2] {
    [
        Event::Transaction(TransactionEntry { signature: sig, payload }),
        Event::Execution(execution(sig, slot, Ok(()))),
    ]
}

/// Ends superblock `id` and rotates the writer to the next one.
fn seal(id: u64) -> Event {
    Event::Superblock(SuperblockSeal { checksum: 0, id, transactions: 0 })
}

/// Serves one read request on a reader run synchronously on the test thread.
///
/// `wrap` is the `ReadRequest` variant to route `params` through — the variants
/// are tuple constructors, so `read(&ledger, sig, ReadRequest::Transaction)`
/// names the request and infers both the parameter and response types. The
/// reader terminates as soon as the single queued request is served.
async fn read<P, R>(
    ledger: &Arc<Ledger>,
    params: P,
    wrap: impl FnOnce(RequestPayload<P, R>) -> ReadRequest,
) -> R {
    let (payload, handle) = RequestPayload::new(params);
    let (tx, reader_rx) = flume::bounded(1);
    tx.send(wrap(payload)).unwrap();
    drop(tx);
    let mut shutdown = ShutdownManager::default();
    LedgerReader::new(ledger.clone(), reader_rx)
        .unwrap()
        .run(shutdown.handle(Service::LedgerReader));
    handle.recv().await.unwrap()
}

/// Reads a full transaction by signature.
async fn read_transaction(ledger: &Arc<Ledger>, sig: Signature) -> Option<TransactionResponse> {
    read(ledger, sig, ReadRequest::Transaction).await.unwrap()
}

/// Reads a block at the requested detail level.
async fn read_block(
    ledger: &Arc<Ledger>,
    slot: Slot,
    details: BlockDetails,
) -> Option<BlockResponse> {
    read(ledger, BlockParams { slot, details }, ReadRequest::Block).await.unwrap()
}

/// Signatures of the block at `slot`; panics if the read returns any other
/// variant or nothing.
async fn block_signatures(ledger: &Arc<Ledger>, slot: Slot) -> Vec<Signature> {
    match read_block(ledger, slot, BlockDetails::Signatures).await {
        Some(BlockResponse::WithSignatures(b)) => b.signatures,
        _ => panic!("expected signatures response for slot {slot}"),
    }
}

/// Streams every replayed blockstore entry after `superblock`.
///
/// The reader runs on its own thread while the test drains the channel, so a
/// small mpsc buffer never deadlocks the `blocking_send` in the replay loop.
async fn replay(ledger: &Arc<Ledger>, superblock: u64) -> Vec<OwnedBlockstoreEntry> {
    let (tx, mut rx) = mpsc::channel(4);
    let (payload, handle) = RequestPayload::new(ReplayParams { superblock, tx });
    let (reader_tx, reader_rx) = flume::bounded(1);
    reader_tx.send(ReadRequest::Replay(payload)).unwrap();
    drop(reader_tx);
    let ledger = ledger.clone();
    let worker = std::thread::spawn(move || {
        let mut shutdown = ShutdownManager::default();
        LedgerReader::new(ledger, reader_rx)
            .unwrap()
            .run(shutdown.handle(Service::LedgerReader));
    });
    let mut entries = Vec::new();
    while let Some(entry) = rx.recv().await {
        entries.push(entry);
    }
    handle.recv().await.unwrap().unwrap();
    worker.join().unwrap();
    entries
}

/// Appends one block at `slot` whose transactions touch `accounts` — one
/// single-account transaction per entry, each paired with its execution —
/// and returns their signatures in append order.
fn block_touching(ledger: &Arc<Ledger>, slot: Slot, accounts: &[Pubkey]) -> Vec<Signature> {
    let mut events = Vec::new();
    let mut signatures = Vec::new();
    for account in accounts {
        let (sig, payload) = transaction(&[*account]);
        events.extend(recorded(sig, payload, slot));
        signatures.push(sig);
    }
    events.push(Event::Block(Block::new(slot, slot as i64 * 100)));
    append(ledger, events);
    signatures
}

/// [`block_touching`] with `count` transactions over distinct fresh accounts,
/// for tests that care about the block, not about who it touches.
fn block_of(ledger: &Arc<Ledger>, slot: Slot, count: usize) -> Vec<Signature> {
    let accounts: Vec<Pubkey> = (0..count).map(|_| Pubkey::new_unique()).collect();
    block_touching(ledger, slot, &accounts)
}

// A transaction and its execution written in one block come back whole through
// every read surface: full transaction bytes, decompressed execution details,
// and the cheap status header — the core append→index→read roundtrip.
#[tokio::test]
async fn test_transaction_roundtrip() {
    let (_dir, ledger) = ledger(u64::MAX);
    let account = Pubkey::new_unique();
    let (sig, bytes) = transaction(&[account]);

    let events = vec![
        Event::Transaction(TransactionEntry {
            signature: sig,
            payload: bytes.clone(),
        }),
        Event::Execution(execution(sig, 5, Ok(()))),
        Event::Block(Block::new(5, 500)),
    ];
    append(&ledger, events);

    let response = read_transaction(&ledger, sig).await.expect("transaction present");
    assert_eq!(response.transaction, *bytes);
    assert_eq!(response.execution.header.slot, 5);
    let details = response.execution.details.expect("details decompressed");
    assert_eq!(details.fee, 5000);
    assert_eq!(details.logs.as_slice(), &["log for 5".to_string()]);

    let status = read(&ledger, sig, ReadRequest::TransactionStatus)
        .await
        .unwrap()
        .expect("status present");
    assert_eq!(status.slot, 5);
    assert!(status.result.is_ok());

    // An unknown signature resolves to nothing on both surfaces.
    let missing = Signature::from([9; 64]);
    assert!(read_transaction(&ledger, missing).await.is_none());
    let status = read(&ledger, missing, ReadRequest::TransactionStatus).await.unwrap();
    assert!(status.is_none(), "unknown signature has no status");
}

// Blockstore payloads may exceed wincode's default 4 MiB preallocation limit.
// Persisting, block reconstruction, and replay all use the ledger-specific
// codec bound and must return the original owned bytes unchanged.
#[tokio::test]
async fn test_large_transaction_roundtrip() {
    let (_dir, ledger) = ledger(u64::MAX);
    let (sig, transaction) = transaction(&[Pubkey::new_unique()]);
    let mut payload = (*transaction).clone();
    payload.resize(10 * MB + 1, 0);
    let payload = Arc::new(payload);

    let events = vec![
        Event::Transaction(TransactionEntry {
            signature: sig,
            payload: payload.clone(),
        }),
        Event::Block(Block::new(1, 0)),
    ];
    append(&ledger, events);

    match read_block(&ledger, 1, BlockDetails::Transactions).await {
        Some(BlockResponse::WithTransactions(block)) => {
            assert_eq!(
                block.transactions.as_slice(),
                std::slice::from_ref(payload.as_ref())
            );
        }
        _ => panic!("expected transactions response"),
    }

    let entries = replay(&ledger, 0).await;
    match entries.first() {
        Some(OwnedBlockstoreEntry::Transaction(transaction)) => {
            assert_eq!(transaction, payload.as_ref());
        }
        _ => panic!("expected replayed transaction entry"),
    }
}

// A transaction stays pending until its execution arrives: sealed into a block
// without one, it is never indexed (a record is never half-written); and an
// execution whose transaction never appeared is silently dropped.
#[tokio::test]
async fn test_pending_requires_execution() {
    let (_dir, ledger) = ledger(u64::MAX);
    let (indexed, indexed_bytes) = transaction(&[Pubkey::new_unique()]);
    let (orphan, orphan_bytes) = transaction(&[Pubkey::new_unique()]);
    let stray = Signature::from([3; 64]);

    let events = vec![
        // Paired transaction: indexed and readable.
        Event::Transaction(TransactionEntry {
            signature: indexed,
            payload: indexed_bytes,
        }),
        Event::Execution(execution(indexed, 1, Ok(()))),
        // Transaction with no execution: written to the blockstore but never
        // indexed, so no read surface can resolve it.
        Event::Transaction(TransactionEntry {
            signature: orphan,
            payload: orphan_bytes,
        }),
        // Execution with no pending transaction: dropped without error.
        Event::Execution(execution(stray, 1, Ok(()))),
        Event::Block(Block::new(1, 0)),
    ];
    append(&ledger, events);

    assert!(read_transaction(&ledger, indexed).await.is_some());
    assert!(read_transaction(&ledger, orphan).await.is_none());
    assert!(read_transaction(&ledger, stray).await.is_none());
}

// Block reads reconstruct exactly the transactions between the previous block
// boundary and this one, at every detail level — the reader derives the block's
// transaction range from the `slot - 1` boundary, so later blocks must not leak
// earlier blocks' transactions.
#[tokio::test]
async fn test_block_detail_levels_partition_transactions() {
    let (_dir, ledger) = ledger(u64::MAX);
    let first = block_of(&ledger, 1, 2);
    let second = block_of(&ledger, 2, 3);

    // Each block reports only its own transactions, in append order.
    assert_eq!(block_signatures(&ledger, 1).await, first);
    assert_eq!(block_signatures(&ledger, 2).await, second);

    // Transactions-only and Full carry the same count without bleed-through.
    match read_block(&ledger, 2, BlockDetails::Transactions).await {
        Some(BlockResponse::WithTransactions(b)) => assert_eq!(b.transactions.len(), 3),
        _ => panic!("expected transactions response"),
    }
    match read_block(&ledger, 2, BlockDetails::Full).await {
        Some(BlockResponse::Full(b)) => {
            assert_eq!(b.transactions.len(), 3);
            assert!(b.transactions.iter().all(|t| t.execution.details.is_some()));
        }
        _ => panic!("expected full response"),
    }

    // The bare boundary carries the block metadata only.
    match read_block(&ledger, 1, BlockDetails::None).await {
        Some(BlockResponse::Bare(block)) => assert_eq!(block.time, 100),
        _ => panic!("expected bare response"),
    }
}

// A sealed superblock stays readable after the writer rotates to a new segment,
// and retention purges the oldest sealed superblock — dropping its transactions
// while preserving the active head and advancing the retained slot range.
#[tokio::test]
async fn test_superblock_rotation_and_retention() {
    // size_limit 0 makes every block boundary trigger a retention pass.
    let (_dir, ledger) = ledger(0);
    let old = block_of(&ledger, 1, 1);
    // Seal superblock 1 and rotate to superblock 2.
    let events = vec![seal(1)];
    append(&ledger, events);
    assert_eq!(ledger.meta.head(), 2);

    // The sealed superblock is still readable through the newer head.
    assert!(read_transaction(&ledger, old[0]).await.is_some());

    // Writing a block into the new head triggers truncation of superblock 1.
    let new = block_of(&ledger, 2, 1);
    assert_eq!(ledger.meta.head(), 2, "active head is never purged");
    assert!(
        ledger.superblocks.read().get(&1).is_none(),
        "oldest sealed segment purged"
    );
    // Its transactions are gone; the head's remain.
    assert!(read_transaction(&ledger, old[0]).await.is_none());
    assert!(read_transaction(&ledger, new[0]).await.is_some());
    // Retention advances the retained range past the purged superblock's end.
    assert_eq!(ledger.meta.range.start.load(Acquire), 2);
}

// A single-block read resolves a slot living in an older sealed superblock, not
// only the active head: each segment's range must be pinned to its own slots so
// a newer segment does not shadow older ones, and the ledger-wide range must
// track the tip so in-range slots pass the guard and out-of-range ones do not.
#[tokio::test]
async fn test_block_read_across_superblocks() {
    let (_dir, ledger) = ledger(u64::MAX);
    let first = block_of(&ledger, 1, 1);
    let events = vec![seal(1)];
    append(&ledger, events);
    block_of(&ledger, 2, 1);

    // Slot 1 lives in the sealed superblock; slot 2 in the head. Both resolve.
    assert_eq!(block_signatures(&ledger, 1).await, first);
    assert!(read_block(&ledger, 2, BlockDetails::None).await.is_some());
    // A slot past the retained tip is rejected by the ledger-wide range guard.
    assert!(read_block(&ledger, 9, BlockDetails::None).await.is_none());
}

// Replay streams superblocks in on-disk order after the last applied seal through
// the active head, so nothing committed after the snapshot is lost on recovery.
// Entries come back exactly as written:
// transactions, their block delimiter, then the seal — and the unsealed head's
// entries have no trailing seal. The read is bounded by each superblock's write
// cursor, so the active head's preallocated tail is not decoded.
#[tokio::test]
async fn test_replay_streams_superblocks_through_active_head() {
    let (_dir, ledger) = ledger(u64::MAX);
    // Two sealed superblocks (slots 1 and 2), then an unsealed head (slot 3).
    block_of(&ledger, 1, 2);
    let events = vec![seal(1)];
    append(&ledger, events);
    block_of(&ledger, 2, 1);
    let events = vec![seal(2)];
    append(&ledger, events);
    block_of(&ledger, 3, 1);

    let entries = replay(&ledger, 0).await;
    use crate::schema::BlockstoreEntry::*;
    let shape: Vec<&str> = entries
        .iter()
        .map(|e| match e {
            Transaction(_) => "tx",
            Block(_) => "block",
            Superblock(_) => "seal",
            Reset(_) => "reset",
        })
        .collect();
    // Superblock 1 (two txns) and superblock 2 (one txn), each ending in its
    // block and seal, followed by the active head (superblock 3: one txn and its
    // block, no seal).
    assert_eq!(
        shape,
        ["tx", "tx", "block", "seal", "tx", "block", "seal", "tx", "block"]
    );
}

// Ledger state survives reopening from disk: committed transactions remain
// readable and a reopened appender resumes at the persisted cursors, appending
// a new block without clobbering the old one.
#[tokio::test]
async fn test_reopen_resumes_state() {
    let dir = tempdir();
    let first = {
        let ledger = Arc::new(Ledger::new(dir.path().to_owned(), u64::MAX).unwrap());
        let sigs = block_of(&ledger, 1, 2);
        sigs[0]
    };

    // Reopen from the same directory.
    let ledger = Arc::new(Ledger::new(dir.path().to_owned(), u64::MAX).unwrap());
    assert!(
        read_transaction(&ledger, first).await.is_some(),
        "prior block survives reopen"
    );

    // A resumed appender writes a second block after the first.
    let second = block_of(&ledger, 2, 1)[0];
    assert!(
        read_transaction(&ledger, first).await.is_some(),
        "old block not clobbered"
    );
    assert!(read_transaction(&ledger, second).await.is_some());
    assert_eq!(ledger.meta.blocks.load(Acquire), 2);
}

// A block-range read returns every block in the range, in ascending slot order,
// even when the range straddles a superblock boundary. The reader walks slots
// descending across superblocks newest-first, so a boundary slot must be handed
// to the older segment instead of being consumed against the newer one.
#[tokio::test]
async fn test_block_range_spans_superblocks() {
    let (_dir, ledger) = ledger(u64::MAX);
    // Slot 1 lands in superblock 1; the seal rotates slots 2 and 3 into
    // superblock 2, so any range over 1..=2 crosses the segment boundary.
    block_of(&ledger, 1, 1);
    let events = vec![seal(1)];
    append(&ledger, events);
    block_of(&ledger, 2, 1);
    block_of(&ledger, 3, 1);

    let slots = async |range: Range<Slot>| {
        read(&ledger, range, ReadRequest::BlockRange)
            .await
            .unwrap()
            .iter()
            .map(|b| b.slot)
            .collect::<Vec<_>>()
    };
    // The full range comes back once each, in order, across the boundary.
    assert_eq!(slots(1..4).await, vec![1, 2, 3]);
    // A sub-range inside one segment returns only its blocks.
    assert_eq!(slots(2..3).await, vec![2]);
    // A tail past the retained tip yields the retained blocks without dropping
    // the boundary slot.
    assert_eq!(slots(1..9).await, vec![1, 2, 3]);
}

// The account index keeps one entry per touching transaction, excludes unrelated
// transactions, and account-signature history pages newest-superblock first with
// exclusive `before`/`until` cursors.
#[tokio::test]
async fn test_account_signatures_history_pagination_and_ordering() {
    let (_dir, ledger) = ledger(u64::MAX);
    let account = Pubkey::new_unique();

    // Two transactions touch `account` in superblock 1, plus a third that does
    // not — the unrelated transaction must stay out of the account's history.
    let sb1 = block_touching(&ledger, 1, &[account, account, Pubkey::new_unique()]);
    let events = vec![seal(1)];
    append(&ledger, events);
    let sb2 = block_touching(&ledger, 2, &[account, account]);

    let read_history = async |pubkey, limit, before, until| {
        let params = AccountSignaturesParams { pubkey, limit, before, until };
        read(&ledger, params, ReadRequest::AccountSignatures).await.unwrap()
    };
    let signatures = |history: &[AccountSignature]| {
        history.iter().map(|s| s.signature).collect::<Vec<Signature>>()
    };

    let expected = vec![sb2[1], sb2[0], sb1[1], sb1[0]];
    let full = read_history(account, 10, None, None).await;
    assert_eq!(signatures(&full), expected);
    assert!(full.iter().all(|s| s.blocktime == s.slot as i64 * 100));

    assert_eq!(
        signatures(&read_history(account, 2, None, None).await),
        expected[..2],
        "`limit` caps results"
    );
    assert!(
        read_history(Pubkey::new_unique(), 10, None, None).await.is_empty(),
        "unmentioned account has no history"
    );

    // Newest-superblock first, newest execution first within each superblock.
    assert_eq!(
        full.iter().map(|s| s.slot).collect::<Vec<_>>(),
        vec![2, 2, 1, 1],
        "newest account history first"
    );

    let history_signatures =
        async |before, until| signatures(&read_history(account, 10, before, until).await);

    // `before` is an exclusive upper bound. The newest cursor yields every
    // older signature across the superblock boundary in newest-to-oldest order.
    assert_eq!(history_signatures(Some(sb2[1]), None).await, expected[1..]);
    // The oldest cursor yields nothing, separating an exclusive bound from an
    // inclusive one.
    assert!(history_signatures(Some(sb1[0]), None).await.is_empty());

    // `until` stops at, and excludes, its signature: the first match yields
    // nothing, the last yields everything above it.
    assert!(history_signatures(None, Some(expected[0])).await.is_empty());
    assert_eq!(
        history_signatures(None, Some(expected[3])).await,
        expected[..3]
    );
}
