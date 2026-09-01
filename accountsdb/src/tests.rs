//! Integration-style unit tests for the two-backend account store.
//!
//! Each test drives a realistic multi-step flow through the public `AccountsDB`
//! surface and reaches into `pub(crate)` internals only to assert *which*
//! backend a given account landed in — the crate's central persisted/volatile
//! invariant that no public method exposes directly.

use std::sync::atomic::Ordering::{Relaxed, Release};

use assert_matches::assert_matches;
use nucleus::testkit::{TempDir, init_tracing, tempdir};
use solana_account::{
    AccountBuilder, AccountMode, AccountSharedData, ReadableAccount, WritableAccount,
};
use solana_pubkey::Pubkey;

use super::*;
use crate::{snapshot::VOLATILE_DB_FILE, store::MIN_REMAINDER, store::index::read_txn};

/// Fresh database on a throwaway directory; the `TempDir` must outlive the db.
fn db() -> (TempDir, AccountsDB) {
    init_tracing();
    let dir = tempdir();
    let db = AccountsDB::new(dir.path()).unwrap();
    (dir, db)
}

/// Owned mutable (persisted) account carrying `data`; its size follows the data.
fn mutable_data(lamports: u64, data: Vec<u8>, owner: &Pubkey) -> AccountSharedData {
    let mut a = AccountSharedData::new(lamports, data.len(), owner);
    a.set_data_from_slice(&data);
    transition(&mut a, AccountMode::Delegated);
    a
}

fn transition(account: &mut AccountSharedData, mode: AccountMode) {
    account.set_lifecycle(mode, account.slot()).unwrap();
}

/// Empty mutable (persisted) account; `owner` defaults to the system program.
fn delegated(lamports: u64) -> AccountSharedData {
    AccountBuilder::default()
        .lamports(lamports)
        .mode(AccountMode::Delegated)
        .build()
}

/// Stores one account, the shape every single-account write below takes.
fn store(db: &AccountsDB, pubkey: Pubkey, account: AccountSharedData) {
    db.store(&[(pubkey, account)]).unwrap();
}

/// Whether a persisted image exists for `pubkey`.
fn in_persisted(db: &AccountsDB, pubkey: &Pubkey) -> bool {
    let mut txn = None;
    db.persisted.contains(&mut txn, pubkey).unwrap()
}

/// Whether a volatile entry exists for `pubkey`.
fn in_volatile(db: &AccountsDB, pubkey: &Pubkey) -> bool {
    db.volatile.contains(pubkey)
}

/// Pubkeys `owner` owns, in iteration order (persisted first, then volatile).
fn program(db: &AccountsDB, owner: &Pubkey) -> Vec<Pubkey> {
    db.program(owner).unwrap().map(|(k, _)| k).collect()
}

/// Balance of the account currently loaded for `pubkey`.
fn lamports(db: &AccountsDB, pubkey: &Pubkey) -> u64 {
    db.loader().load(pubkey).unwrap().unwrap().lamports()
}

/// Loads the account currently stored for `pubkey`.
///
/// A persisted account comes back as a *borrowed* image and a volatile one as
/// *owned*; storing the loaded value back is how the engine drives mode changes
/// through the routing layer (a freshly built owned account with a
/// non-authoritative mode is filtered out of the persisted backend entirely).
fn reload(db: &AccountsDB, pubkey: &Pubkey) -> AccountSharedData {
    db.loader().load(pubkey).unwrap().unwrap()
}

/// Closes `pubkey`, deleting it from whichever backend currently holds it.
///
/// Goes through the load→mutate→store path so the account is a *borrowed* image
/// the routing layer will actually evict (see [`reload`]).
fn close(db: &AccountsDB, pubkey: &Pubkey) {
    let mut acc = reload(db, pubkey);
    if acc.is(AccountMode::Delegated) {
        transition(&mut acc, AccountMode::Transient);
    }
    if acc.is(AccountMode::Transient) {
        transition(&mut acc, AccountMode::ReadOnly);
    }
    transition(&mut acc, AccountMode::Closed);
    store(db, *pubkey, acc);
}

/// Allocation high-water mark of the persisted store, in storage units.
fn cursor(db: &AccountsDB) -> u32 {
    db.persisted.storage.cursor()
}

/// Current persisted offset for one live account.
fn offset(db: &AccountsDB, pubkey: &Pubkey) -> impl Copy + PartialEq + use<> {
    let mut txn = None;
    let txn = read_txn(db.persisted.index.env(), &mut txn).unwrap();
    db.persisted.index.offset(pubkey, txn).unwrap().unwrap()
}

/// Defragments until a pass makes no change and returns the reclaimed total.
fn defrag_to_stable(db: &AccountsDB) -> u32 {
    let mut total = 0;
    loop {
        // SAFETY: the test is the sole owner of the store during defrag.
        let pass = unsafe { db.persisted.defragment() }.unwrap();
        total += pass.reclaimed;
        if !pass.changed() {
            return total;
        }
    }
}

/// Builds a mutable account with an exact persisted span.
fn mutable_units(lamports: u64, units: u32, owner: &Pubkey) -> AccountSharedData {
    let account = mutable_at_least(lamports, units, owner);
    assert_eq!(account.owned().units(), units);
    account
}

/// Builds the smallest mutable account spanning at least `units` storage units.
fn mutable_at_least(lamports: u64, units: u32, owner: &Pubkey) -> AccountSharedData {
    (0..=units as usize * solana_account::STORAGE_UNIT)
        .map(|len| mutable_data(lamports, vec![0; len], owner))
        .find(|account| account.owned().units() >= units)
        .unwrap()
}

// Routing, both eviction directions, owner remap and Closed/reset handling in
// one flow — the persisted-vs-volatile invariant is what this whole crate
// exists to enforce.
#[test]
fn test_routing_and_persistence_flips() {
    let (_dir, db) = db();
    let (p, q) = (Pubkey::new_unique(), Pubkey::new_unique());
    let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());

    let aacc = AccountBuilder::default().lamports(10).owner(p).mode(AccountMode::Delegated);
    let bacc = AccountBuilder::default().lamports(20).owner(p);
    // `a` is authoritative, `b` is non-authoritative; both are owned by `p`.
    db.store(&[(a, aacc.build()), (b, bacc.build())]).unwrap();
    assert!(in_persisted(&db, &a) && !in_volatile(&db, &a));
    assert!(in_volatile(&db, &b) && !in_persisted(&db, &b));

    // Loader reads across both backends; contains agrees.
    let loader = db.loader();
    assert_eq!(loader.load(&a).unwrap().unwrap().lamports(), 10);
    assert_eq!(loader.load(&b).unwrap().unwrap().lamports(), 20);
    assert!(loader.contains(&a).unwrap() && loader.contains(&b).unwrap());
    assert!(!loader.contains(&Pubkey::new_unique()).unwrap());
    drop(loader);

    // Persisted account is yielded before the volatile one.
    assert_eq!(program(&db, &p), vec![a, b]);

    // Loading a persisted account returns a borrowed image; mutating its owner
    // and re-storing must commit in place and remap the program index.
    let mut borrowed = reload(&db, &a);
    borrowed.set_owner(q);
    store(&db, a, borrowed);
    assert_eq!(program(&db, &p), vec![b]); // `a` left p's set
    assert_eq!(program(&db, &q), vec![a]); // and joined q's

    // Transient is immutable to programs but remains persistent while its
    // lifecycle state is unresolved.
    let mut flip = reload(&db, &a);
    transition(&mut flip, AccountMode::Transient);
    flip.set_lamports(30);
    store(&db, a, flip);
    let transient = reload(&db, &a);
    assert!(!transient.mutable());
    assert!(in_persisted(&db, &a) && !in_volatile(&db, &a));
    assert_eq!(lamports(&db, &a), 30);

    // Resolving to ReadOnly evicts the persisted copy into volatile.
    let mut flip = reload(&db, &a);
    transition(&mut flip, AccountMode::ReadOnly);
    store(&db, a, flip);
    assert!(!in_persisted(&db, &a) && in_volatile(&db, &a));
    assert_eq!(lamports(&db, &a), 30);

    // ReadOnly → Delegated evicts it back into persisted.
    let mut flip = reload(&db, &a);
    transition(&mut flip, AccountMode::Delegated);
    store(&db, a, flip);
    assert!(in_persisted(&db, &a) && !in_volatile(&db, &a));

    // Closing removes it from both backends.
    close(&db, &a);
    assert!(!in_persisted(&db, &a) && !in_volatile(&db, &a));

    // reset() drops volatile mirror only; persisted state is authoritative.
    let c = Pubkey::new_unique();
    store(&db, c, mutable_data(50, vec![], &p));
    db.reset();
    assert!(!in_volatile(&db, &b));
    assert!(in_persisted(&db, &c));
}

// Both migration directions remove the source image and owner mapping, retain
// the account contents across reopen, and recycle persisted storage.
#[test]
fn test_store_kind_migration_invariants() {
    let dir = tempdir();
    let (persisted_owner, volatile_owner, reuse_owner) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    let (key, reuse) = (Pubkey::new_unique(), Pubkey::new_unique());
    let data = vec![1, 2, 3, 4];
    let assert_migrated = |db: &AccountsDB, persisted: bool, owner: Pubkey| {
        assert_eq!(in_persisted(db, &key), persisted);
        assert_eq!(in_volatile(db, &key), !persisted);
        assert_eq!(program(db, &owner), vec![key]);
        let account = reload(db, &key);
        assert_eq!(account.owner(), &owner);
        assert_eq!(account.lamports(), 20);
        assert_eq!(account.data(), data);
    };

    {
        let db = AccountsDB::new(dir.path()).unwrap();
        store(&db, key, mutable_data(10, data.clone(), &persisted_owner));
        let base = cursor(&db);

        let mut account = reload(&db, &key);
        transition(&mut account, AccountMode::Transient);
        transition(&mut account, AccountMode::ReadOnly);
        account.set_owner(volatile_owner);
        account.set_lamports(20);
        store(&db, key, account);

        assert_migrated(&db, false, volatile_owner);
        assert!(program(&db, &persisted_owner).is_empty());

        // A same-sized persisted account must reuse the span released by the
        // migration instead of extending the mmap.
        store(&db, reuse, mutable_data(30, data.clone(), &reuse_owner));
        assert_eq!(cursor(&db), base);

        db.dump(None).unwrap();
    }

    {
        let db = AccountsDB::new(dir.path()).unwrap();
        assert_migrated(&db, false, volatile_owner);
        assert!(program(&db, &persisted_owner).is_empty());

        let mut account = reload(&db, &key);
        transition(&mut account, AccountMode::Delegated);
        account.set_owner(persisted_owner);
        store(&db, key, account);

        assert_migrated(&db, true, persisted_owner);
        assert!(program(&db, &volatile_owner).is_empty());

        db.flush(true).unwrap();
        // Persist a stale volatile copy if cleanup regresses, so the final open
        // can verify source-store cleanup rather than merely losing memory state.
        db.dump(None).unwrap();
    }

    let db = AccountsDB::new(dir.path()).unwrap();
    assert_migrated(&db, true, persisted_owner);
    assert!(program(&db, &volatile_owner).is_empty());
    assert_eq!(program(&db, &reuse_owner), vec![reuse]);
}

// Persisted state and metadata survive a close/reopen, and validate() accepts
// the synced checksum.
#[test]
fn test_persistence_reopen_and_validate() {
    let dir = tempdir();
    let keys: Vec<Pubkey> = (0..8).map(|_| Pubkey::new_unique()).collect();

    let (checksum, before) = {
        let db = AccountsDB::new(dir.path()).unwrap();
        for (i, k) in keys.iter().enumerate() {
            store(&db, *k, delegated(100 + i as u64));
        }
        let discarded = Pubkey::new_unique();
        store(&db, discarded, delegated(0));
        close(&db, &discarded);
        db.set_slot(42).unwrap();
        // Sync the checksum into the header so a reopen can validate against it.
        db.persisted.flush(true).unwrap();
        assert!(db.validate().is_ok());
        (db.checksum(), cursor(&db))
    };

    let mut db = AccountsDB::new(dir.path()).unwrap();
    assert!(db.validate().is_ok());
    let reclaimed = db.compact().unwrap();
    assert_eq!(reclaimed, before - cursor(&db));
    assert!(reclaimed > 0);
    for (i, k) in keys.iter().enumerate() {
        assert_eq!(lamports(&db, k), 100 + i as u64);
    }
    assert_eq!(db.slot(), 42);
    assert_eq!(db.checksum(), checksum);
    assert!(db.validate().is_ok());
}

// A clean-shutdown dump lives in the active tree, is restored on the next open,
// and is then removed so volatile state returns to its in-memory-only form.
#[test]
fn test_dump_restores_volatile_on_reopen() {
    let dir = tempdir();
    let key = Pubkey::new_unique();
    let active = AccountsDB::directory(dir.path());
    let dump = active.join(VOLATILE_DB_FILE);

    {
        let db = AccountsDB::new(dir.path()).unwrap();
        let account = AccountBuilder::default().lamports(42).mode(AccountMode::ReadOnly).build();
        store(&db, key, account);
        db.dump(None).unwrap();
        assert!(dump.exists(), "dump is written into the active tree");
    }

    let db = AccountsDB::new(dir.path()).unwrap();
    assert_eq!(lamports(&db, &key), 42);
    assert!(in_volatile(&db, &key));
    assert!(!dump.exists(), "restored dump is consumed on open");
}

// A freed span is reused for a same-sized insert instead of growing the file;
// genuinely new accounts still extend it.
#[test]
fn test_freelist_reuse_and_growth() {
    let (_dir, db) = db();
    let stats = || {
        let s = db.persisted.storage.stats();
        (s.allocs.load(Relaxed), s.reallocs.load(Relaxed))
    };

    let k1 = Pubkey::new_unique();
    store(&db, k1, delegated(1));
    let base = cursor(&db);
    let (_, reallocs) = stats();

    // Close k1 (returns its span to the freelist), then insert a same-sized
    // account: it should land in the freed span without advancing the cursor.
    close(&db, &k1);
    store(&db, Pubkey::new_unique(), delegated(2));
    assert_eq!(cursor(&db), base);
    assert_eq!(stats().1, reallocs + 1);

    // Fresh accounts have no reusable span, so the file grows.
    let (allocs, _) = stats();
    for _ in 0..8 {
        store(&db, Pubkey::new_unique(), delegated(1));
    }
    assert!(cursor(&db) > base);
    assert!(stats().0 > allocs);
}

// Defragmentation reclaims interior holes while preserving every live account's
// content, ownership index, and checksum.
#[test]
fn test_defragment_preserves_live_accounts() {
    let (_dir, db) = db();
    let owner = Pubkey::new_unique();
    let keys: Vec<Pubkey> = (0..16).map(|_| Pubkey::new_unique()).collect();
    for (i, k) in keys.iter().enumerate() {
        store(&db, *k, mutable_data(100 + i as u64, vec![], &owner));
    }

    // Punch alternating holes; keep the survivors for later comparison.
    let mut live = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        if i % 2 == 0 {
            close(&db, k);
        } else {
            live.push((*k, 100 + i as u64));
        }
    }
    db.persisted.flush(true).unwrap();
    let checksum = db.checksum();
    let before = cursor(&db);

    let reclaimed = defrag_to_stable(&db);
    assert_eq!(reclaimed, before - cursor(&db));
    assert!(cursor(&db) < before);

    // Every survivor still loads unchanged and remains program-indexed.
    for (k, lam) in &live {
        let acc = db.loader().load(k).unwrap().unwrap();
        assert_eq!(acc.lamports(), *lam);
        assert_eq!(acc.owner(), &owner);
    }
    let mut owned = program(&db, &owner);
    owned.sort();
    let mut expected: Vec<Pubkey> = live.iter().map(|(k, _)| *k).collect();
    expected.sort();
    assert_eq!(owned, expected);

    // Relocating images must not change the content checksum.
    db.persisted.flush(true).unwrap();
    assert_eq!(db.checksum(), checksum);
}

/// Exact and thresholded best-fit packing updates component holes correctly,
/// while deferred source holes become usable only by a later committed pass.
#[test]
fn test_defragment_best_fit_and_deferred_holes() {
    let owner = Pubkey::new_unique();
    let small = mutable_units(1, 21, &owner);
    let small_units = small.owned().units();
    let medium = mutable_at_least(2, MIN_REMAINDER, &owner);
    let medium_units = medium.owned().units();

    // Two adjacent component holes form one run. The small tail account leaves
    // a useful remainder, which the following account consumes exactly.
    {
        let (_dir, db) = db();
        let (small_hole, medium_hole, anchor, medium_key, small_key) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        store(&db, small_hole, small.clone());
        store(&db, medium_hole, medium.clone());
        store(&db, anchor, small.clone());
        store(&db, medium_key, medium.clone());
        store(&db, small_key, small.clone());
        let medium_dst = offset(&db, &medium_hole);
        let small_dst = offset(&db, &small_hole);
        close(&db, &small_hole);
        close(&db, &medium_hole);

        let pass = unsafe { db.persisted.defragment() }.unwrap();
        assert_eq!(pass.moved, 2);
        assert_eq!(pass.reclaimed, small_units + medium_units);
        assert!(offset(&db, &medium_key) == medium_dst);
        assert!(offset(&db, &small_key) == small_dst);
        assert_eq!(lamports(&db, &medium_key), 2);
        assert_eq!(lamports(&db, &small_key), 1);
        assert_eq!(lamports(&db, &anchor), 1);

        let before = cursor(&db);
        store(&db, Pubkey::new_unique(), small.clone());
        assert_eq!(cursor(&db), before + small_units);
    }

    // Exact fit wins first. The next account skips a hole whose remainder is
    // just below the threshold. Two accounts instead use a wider hole and
    // publish a useful suffix.
    {
        let (_dir, db) = db();
        let short_remainder = (MIN_REMAINDER - 1) & !1;
        let near_units = small_units + short_remainder;
        let near = mutable_units(3, near_units, &owner);
        let wide = mutable_units(4, 2 * small_units + medium_units, &owner);
        let (
            near_hole,
            anchor_a,
            wide_hole,
            anchor_b,
            exact_hole,
            anchor_c,
            wide_key_a,
            wide_key_b,
            exact_key,
        ) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        store(&db, near_hole, near);
        store(&db, anchor_a, small.clone());
        store(&db, wide_hole, wide);
        store(&db, anchor_b, small.clone());
        store(&db, exact_hole, small.clone());
        store(&db, anchor_c, small.clone());
        store(&db, wide_key_a, small.clone());
        store(&db, wide_key_b, small.clone());
        store(&db, exact_key, small.clone());
        let near_dst = offset(&db, &near_hole);
        let wide_dst = offset(&db, &wide_hole);
        let exact_dst = offset(&db, &exact_hole);
        close(&db, &near_hole);
        close(&db, &wide_hole);
        close(&db, &exact_hole);

        let pass = unsafe { db.persisted.defragment() }.unwrap();
        assert_eq!(pass.moved, 3);
        assert_eq!(pass.reclaimed, 63);
        assert!(offset(&db, &wide_key_b) == wide_dst);
        assert!(offset(&db, &exact_key) == exact_dst);

        let before = cursor(&db);
        store(&db, Pubkey::new_unique(), medium.clone());
        assert_eq!(cursor(&db), before);
        let near_key = Pubkey::new_unique();
        store(&db, near_key, mutable_units(5, near_units, &owner));
        assert_eq!(cursor(&db), before);
        assert!(offset(&db, &near_key) == near_dst);
    }

    // The first pass moves only the middle account. Its source joins the next
    // hole after commit, and public startup compaction exhausts later passes.
    {
        let (_dir, mut db) = db();
        let second = mutable_units(3, 2 * medium_units - small_units, &owner);
        let (first_hole, middle, second_hole, tail) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        store(&db, first_hole, small.clone());
        store(&db, middle, small.clone());
        store(&db, second_hole, second);
        store(&db, tail, medium.clone());
        let middle_dst = offset(&db, &first_hole);
        let tail_dst = offset(&db, &middle);
        close(&db, &first_hole);
        close(&db, &second_hole);
        let before = cursor(&db);

        let pass = unsafe { db.persisted.defragment() }.unwrap();
        assert_eq!((pass.moved, pass.reclaimed), (1, 0));
        assert_eq!(cursor(&db), before);
        assert!(offset(&db, &middle) == middle_dst);

        assert_eq!(db.compact().unwrap(), 2 * medium_units);
        assert!(offset(&db, &tail) == tail_dst);
        assert_eq!(lamports(&db, &middle), 1);
        assert_eq!(lamports(&db, &tail), 2);

        let before = cursor(&db);
        store(&db, Pubkey::new_unique(), small.clone());
        assert_eq!(cursor(&db), before + small_units);
    }
}

// A snapshot is a self-contained tree: reopening it restores persisted accounts
// and bootstraps the volatile store from volatile.db, which is then consumed.
#[test]
fn test_snapshot_export_and_volatile_restore() {
    let src = tempdir();
    let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());

    let snapshot = {
        let db = AccountsDB::new(src.path()).unwrap();
        let aacc = AccountBuilder::default().lamports(10).mode(AccountMode::Delegated);
        let bacc = AccountBuilder::default().lamports(20).mode(AccountMode::ReadOnly);
        db.store(&[(a, aacc.build()), (b, bacc.build())]).unwrap();
        // SAFETY: the test holds exclusive access to the store.
        unsafe { db.snapshot(1) }.unwrap()
    };

    // Adopt the snapshot as a new database's active tree.
    let dst = tempdir();
    let active = AccountsDB::directory(dst.path());
    std::fs::rename(&snapshot, &active).unwrap();
    let db = AccountsDB::new(dst.path()).unwrap();

    assert_eq!(lamports(&db, &a), 10);
    assert_eq!(lamports(&db, &b), 20);
    assert!(in_persisted(&db, &a));
    assert!(in_volatile(&db, &b));
    // The volatile payload is single-sourced back into memory on open.
    assert!(!active.join(VOLATILE_DB_FILE).exists());

    // Backup renames the active tree out and back.
    let saved = db.backup(BackupOp::Save).unwrap();
    assert!(saved.exists() && !active.exists());
    db.backup(BackupOp::Restore).unwrap();
    assert!(active.exists());
}

// validate() flags a persisted checksum that no longer matches the images.
#[test]
fn test_corruption_detection() {
    let (_dir, db) = db();
    for _ in 0..4 {
        store(&db, Pubkey::new_unique(), delegated(1));
    }
    db.persisted.flush(true).unwrap();
    assert!(db.validate().is_ok());

    // Corrupt the recorded checksum; recomputation must no longer agree.
    db.persisted.meta().checksum.store(0xDEAD_BEEF, Release);
    assert_matches!(db.validate(), Err(AccountsDBError::Corruption));
}

// Several freed spans of one size accumulate as duplicates under a single
// freelist key and are all reissued before the file grows — the N>1 duplicate
// case a broken DUP config silently loses.
#[test]
fn test_freelist_multi_duplicate_reuse() {
    let (_dir, db) = db();
    let reallocs = || db.persisted.storage.stats().reallocs.load(Relaxed);

    const N: usize = 6;
    let keys: Vec<Pubkey> = (0..N).map(|_| Pubkey::new_unique()).collect();
    for k in &keys {
        store(&db, *k, delegated(1));
    }
    let base = cursor(&db);
    // Close them all: N same-size spans return to the freelist as N duplicates.
    for k in &keys {
        close(&db, k);
    }
    let reused = reallocs();

    // Each of N fresh same-size inserts must land in a freed span, so the cursor
    // never advances and every insert is a reuse.
    for _ in 0..N {
        store(&db, Pubkey::new_unique(), delegated(2));
    }
    assert_eq!(cursor(&db), base);
    assert_eq!(reallocs(), reused + N as u64);
}

// An immutable account changing owner is re-homed in the volatile program index
// and the now-empty old owner set is pruned.
#[test]
fn test_volatile_owner_remap() {
    let (_dir, db) = db();
    let (x, y) = (Pubkey::new_unique(), Pubkey::new_unique());
    let k = Pubkey::new_unique();

    store(
        &db,
        k,
        AccountBuilder::default().lamports(10).owner(x).build(),
    );
    assert_eq!(program(&db, &x), vec![k]);

    // Re-store the volatile account under a new owner.
    let mut moved = reload(&db, &k);
    moved.set_owner(y);
    store(&db, k, moved);

    assert_eq!(program(&db, &x), Vec::<Pubkey>::new()); // old set pruned
    assert_eq!(program(&db, &y), vec![k]);
    assert!(in_volatile(&db, &k));
}

// The freelist reuses a span only on an exact size match, and accounts of mixed
// sizes survive defragmentation with their data intact.
#[test]
fn test_variable_sizes_and_exact_freelist() {
    let (_dir, db) = db();
    let owner = Pubkey::new_unique();
    let reallocs = || db.persisted.storage.stats().reallocs.load(Relaxed);

    // A freed large span cannot satisfy a smaller allocation: sizes differ, so
    // the small insert allocates fresh rather than reusing the hole.
    let big = Pubkey::new_unique();
    store(&db, big, mutable_data(1, vec![0; 4096], &owner));
    close(&db, &big);
    let before = reallocs();
    let small = Pubkey::new_unique();
    store(&db, small, mutable_data(2, vec![0; 64], &owner));
    assert_eq!(reallocs(), before); // size mismatch -> no reuse

    // Store a spread of sizes with distinct data, punch an interior hole, then
    // defragment and confirm every survivor keeps its exact bytes.
    let sizes = [8usize, 512, 100, 4096, 1];
    let mut live = Vec::new();
    for (i, &space) in sizes.iter().enumerate() {
        let k = Pubkey::new_unique();
        let data: Vec<u8> = (0..space).map(|b| (b as u8).wrapping_add(i as u8)).collect();
        store(&db, k, mutable_data(i as u64, data.clone(), &owner));
        live.push((k, data));
    }
    close(&db, &small);

    defrag_to_stable(&db);
    for (k, data) in &live {
        assert_eq!(
            db.loader().load(k).unwrap().unwrap().data(),
            data.as_slice()
        );
    }
}

// The checksum hashes accounts in pubkey order, so it depends only on content —
// not on insertion order or the resulting on-disk offsets.
#[test]
fn test_checksum_order_independent() {
    let keys: Vec<Pubkey> = (0..8).map(|_| Pubkey::new_unique()).collect();

    let checksum = |order: &[usize]| {
        let (_dir, db) = db();
        for &i in order {
            store(&db, keys[i], delegated(100 + i as u64));
        }
        db.persisted.flush(true).unwrap();
        db.checksum()
    };

    let forward: Vec<usize> = (0..keys.len()).collect();
    let reversed: Vec<usize> = (0..keys.len()).rev().collect();
    assert_eq!(checksum(&forward), checksum(&reversed));
}

// 2 MiB accounts overflow the initial storage block, forcing the file to grow;
// removing half then defragmenting reclaims the large holes and shrinks the
// cursor back — growth and compaction over multi-megabyte images.
#[test]
fn test_large_accounts_growth_and_defrag() {
    const SIZE: usize = 2 << 20; // 2 MiB of data per account
    const COUNT: usize = 12; // ~24 MiB total, past the 16 MiB test block

    let (_dir, db) = db();
    let owner = Pubkey::new_unique();
    let resizes = || db.persisted.storage.stats().resizes.load(Relaxed);
    let baseline = resizes();

    // Distinct fill byte per account so content is verifiable without retaining
    // the expected bytes.
    let keys: Vec<Pubkey> = (0..COUNT).map(|_| Pubkey::new_unique()).collect();
    for (i, k) in keys.iter().enumerate() {
        store(&db, *k, mutable_data(i as u64, vec![i as u8; SIZE], &owner));
    }
    // Crossing the initial block must have grown the file.
    assert!(resizes() > baseline);

    // Close every other account to punch large interior holes.
    let mut live = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        if i % 2 == 0 {
            close(&db, k);
        } else {
            live.push((*k, i as u8));
        }
    }
    let before = cursor(&db);

    let reclaimed = defrag_to_stable(&db);
    assert_eq!(reclaimed, before - cursor(&db));
    assert!(cursor(&db) < before);

    // Every survivor keeps its full 2 MiB image byte-for-byte.
    for (k, fill) in &live {
        let acc = db.loader().load(k).unwrap().unwrap();
        assert_eq!(acc.data().len(), SIZE);
        assert!(acc.data().iter().all(|&b| b == *fill));
    }
}
