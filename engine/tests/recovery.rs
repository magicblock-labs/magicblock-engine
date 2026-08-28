//! Full-engine replay recovery — the engine's most distinctive orchestration.
//! After an accountsdb inconsistency the keeper restores an older archived snapshot,
//! leaving durable state behind the ledger tip; the engine then spins a temporary
//! replay sequencer to re-execute the retained ledger entries and rebuild the
//! missing state, checksum-verified at each sealed superblock. Nothing below the
//! engine wires this end to end. Covered here: the healthy restart that must not
//! recover, the replay that crosses a sealed checksum and succeeds, and the
//! replay that diverges from one and must refuse to start.
#![cfg(test)]

use std::{path::PathBuf, time::Duration};

use engine::{EngineError, ReplayError, testkit::TestEngine};
use keeper::testkit::{corrupt, load_v42_data, signed_view, store_v42};
use nucleus::ledger::ACCOUNTSDB_SNAPSHOT_FILE;
use solana_account::AccountMode;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::TransactionError;
use tokio::time;
use v42_calculator_interface::builder::Expr as E;

/// Commits `K = value` through a full transaction and seals the following
/// superblock, returning its archived snapshot path.
async fn commit_and_seal(te: &mut TestEngine, key: Pubkey, value: i64) -> PathBuf {
    te.execute(&[E::lit(value).compose(key, &[])]).await.unwrap();
    te.seal_and_archive().await
}

/// Verifies a startup-restored terminal status rejects the original bytes.
async fn assert_restored_signature(
    te: &TestEngine,
    key: Pubkey,
    signature: Signature,
    transaction: Vec<u8>,
    value: i64,
) {
    let status = te
        .transactions()
        .status(signature)
        .await
        .expect("status lookup succeeds")
        .expect("terminal status is restored");
    assert!(
        status.result.is_ok(),
        "successful terminal status is available"
    );

    let result = te
        .transaction(transaction)
        .expect("persisted transaction remains valid")
        .execute()
        .await
        .expect("duplicate submission returns a terminal status");
    assert_eq!(result, Err(TransactionError::AlreadyProcessed));
    assert_eq!(
        load_v42_data(te, key),
        Some(value),
        "duplicate transaction was not executed again"
    );
}

/// Proves snapshot-tail replay rebuilds state and refreshes processed signatures.
///
/// Dropping superblock 2's archive forces the restore back onto snapshot 1, so
/// re-executing B crosses superblock 2's sealed checksum before C is rebuilt
/// from the unsealed head. C must then remain deduplicated after replay.
#[tokio::test(flavor = "multi_thread")]
async fn replay_rebuilds_state_after_counter_lag() {
    let mut te = TestEngine::new().await;
    let key = store_v42(&te, 0, AccountMode::Delegated);

    // A: K = 10 sealed into superblock 1, whose snapshot the restore lands on.
    let s1 = commit_and_seal(&mut te, key, 10).await;
    assert!(s1.exists(), "archived accountsdb snapshot exists on disk");
    assert!(
        s1.ends_with(ACCOUNTSDB_SNAPSHOT_FILE),
        "archive is the compressed accountsdb tarball"
    );
    // B: K = 20 sealed into superblock 2; C increments K and lives only in the
    // ledger's unsealed head, past every archived snapshot.
    let s2 = commit_and_seal(&mut te, key, 20).await;
    let (signature, transaction) =
        signed_view(&te, None, (E::acc(0) + E::lit(1)).compose(key, &[]));
    let transaction = transaction.inner_data().as_ref().clone();
    te.execute(transaction.clone()).await.expect("C commits");
    te.advance(2).await;
    let (dirs, authority) = te.close().await;

    // Lag only accountsdb's durable checkpoint in the closed store, preserving
    // valid account content and its checksum.
    corrupt(dirs.accounts.path(), 32, 2);

    // Drop the newest archive so recovery falls back to snapshot 1 (K = 10) and
    // replays both a sealed successor and the unsealed ledger head.
    std::fs::remove_file(&s2).unwrap();

    let te2 = TestEngine::with(dirs, authority).await;
    assert_eq!(
        load_v42_data(&te2, key),
        Some(21),
        "both post-snapshot mutations were rebuilt purely from ledger replay"
    );
    assert_restored_signature(&te2, key, signature, transaction, 21).await;
    // The temporary replay sequencer must hand off to a working live one.
    te2.execute(&[E::lit(1).compose(key, &[])])
        .await
        .expect("engine is live after replay");

    te2.close().await;
}

// A mutation that bypasses the ledger is sealed into superblock 2's checksum but
// can never be rebuilt by replay, so the reopen must refuse to come up with
// `StateMismatch` rather than run on quietly diverged state.
#[tokio::test(flavor = "multi_thread")]
async fn replay_aborts_on_checksum_mismatch() {
    let mut te = TestEngine::new().await;
    let key = store_v42(&te, 0, AccountMode::Delegated);
    commit_and_seal(&mut te, key, 10).await;
    // Direct store: lands in persisted state (and superblock 2's checksum)
    // without a ledger entry.
    store_v42(&te, 7, AccountMode::Delegated);
    let s2 = commit_and_seal(&mut te, key, 20).await;
    let (dirs, authority) = te.close().await;

    corrupt(dirs.accounts.path(), 8, 0xABAB_ABAB_ABAB_ABAB);
    std::fs::remove_file(&s2).unwrap();

    let result = time::timeout(
        Duration::from_secs(4),
        TestEngine::try_with(dirs, authority),
    )
    .await
    .expect("replay aborts in time");
    let error = result.err().expect("diverged checksum refuses startup");
    assert!(
        matches!(error, EngineError::Replay(ReplayError::StateMismatch)),
        "unexpected startup error: {error:?}"
    );
}

/// Proves a clean restart restores processed signatures without re-execution.
///
/// Persisted state reopens as-is with the clean-shutdown volatile dump. A failed
/// execution still counts on both durable sides without writing accounts. The
/// exact successful transaction remains terminal and is rejected on resubmission.
#[tokio::test(flavor = "multi_thread")]
async fn clean_restart_reopens_persisted_and_volatile_state() {
    let mut te = TestEngine::new().await;
    let key = store_v42(&te, 0, AccountMode::Delegated);
    commit_and_seal(&mut te, key, 10).await;
    te.execute(&[E::lit(20).compose(key, &[])]).await.unwrap();
    let failed = (E::lit(i64::MIN) - E::lit(1)).compose(key, &[]);
    assert!(
        te.execute(&[failed]).await.is_err(),
        "overflow execution fails"
    );
    assert_eq!(
        load_v42_data(&te, key),
        Some(20),
        "failed execution writes no state"
    );
    let (signature, transaction) =
        signed_view(&te, None, (E::acc(0) + E::lit(1)).compose(key, &[]));
    let transaction = transaction.inner_data().as_ref().clone();
    te.execute(transaction.clone()).await.expect("increment commits");
    te.advance(1).await;
    let direct = store_v42(&te, 7, AccountMode::Delegated);
    let volatile = store_v42(&te, 8, AccountMode::ReadOnly);
    let (dirs, authority) = te.close().await;

    let te2 = TestEngine::with(dirs, authority).await;
    assert_eq!(
        load_v42_data(&te2, key),
        Some(21),
        "persisted tip state reopened as-is"
    );
    assert_restored_signature(&te2, key, signature, transaction, 21).await;
    assert_eq!(
        load_v42_data(&te2, direct),
        Some(7),
        "ledger-invisible account intact, so no snapshot was restored"
    );
    assert_eq!(
        load_v42_data(&te2, volatile),
        Some(8),
        "clean shutdown restores volatile state"
    );

    te2.close().await;
}
