//! Account-mutability enforcement at the engine boundary.
//!
//! The SVM lets a program write any account it owns; the engine's post-execution
//! guard (`validate_access`) is what rejects writes to accounts that are not in a
//! mutable mode, unless the whole transaction is privileged (every instruction
//! targets MagicRoot). These black-box tests drive the full engine and assert both
//! that the rejection surfaces the right error and that the illegal write never
//! commits. A second enforcement path — MagicRoot's own `post_finalize` check —
//! is covered by `post_finalize_immutable_action_is_rejected`. MagicRoot also
//! honors caller-supplied protection flags and lifecycle transitions.
#![cfg(test)]

use engine::{EngineError, testkit::TestEngine};
use keeper::testkit::{load_v42_data, load_v42_lamports, signed_view, store_v42, v42_builder};
use magic_root_interface::MagicRootInstruction;
use solana_account::{AccountFieldPatch, AccountMode, ReadableAccount, StateFlags};
use solana_instruction::Instruction;
use solana_instruction_error::InstructionError;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::TransactionError;
use v42_calculator_interface::builder::{Expr as E, transfer};

/// Complete v42 account replacement at an explicit non-default slot.
fn compose_v42_replacement(key: Pubkey, mode: AccountMode, slot: u64) -> Vec<Instruction> {
    let account = v42_builder(0, mode).slot(slot).build();
    MagicRootInstruction::compose_account(key, account).unwrap()
}

/// Marks an account as protected against later MagicRoot mutation.
async fn protect(te: &TestEngine, key: Pubkey) {
    te.account(key)
        .patch(vec![AccountFieldPatch::Flag {
            flag: StateFlags::PROTECTED,
            val: true,
        }])
        .await
        .unwrap();
}

/// Proves a caller-supplied protection flag rejects a newer complete
/// replacement without changing stored state.
#[tokio::test(flavor = "multi_thread")]
async fn protected_account_rejects_newer_replacement() {
    let te = TestEngine::new().await;
    let key = Pubkey::new_unique();
    te.accounts()
        .store(&[(key, v42_builder(0, AccountMode::ReadOnly).slot(2).build())])
        .unwrap();
    protect(&te, key).await;

    let replacement = v42_builder(9, AccountMode::ReadOnly).slot(3).build();
    let error = te
        .account(key)
        .update(replacement)
        .await
        .expect_err("protected replacement is rejected");
    assert!(matches!(
        error,
        EngineError::TransactionExecution(TransactionError::InstructionError(
            0,
            InstructionError::PrivilegeEscalation
        ))
    ));

    let unchanged = te.get_account(key).expect("protected account remains");
    assert_eq!(unchanged.slot(), 2);
    assert_eq!(unchanged.data(), &0_i64.to_le_bytes());

    te.close().await;
}

// The SVM permits the v42 program to write accounts it owns, but the guard
// rejects the commit and discards the mutation whenever the account is immutable
// and the transaction is not privileged. Two branches: a writable operand yields
// InvalidWritableAccount, the fee payer itself yields InvalidAccountForFee. A
// delegated (mutable) account is the positive control.
#[tokio::test(flavor = "multi_thread")]
async fn immutable_writes_are_rejected_and_not_committed() {
    let te = TestEngine::new().await;

    // A writable, non-payer immutable source: the transfer dirties both balance
    // fields before the guard rejects the source account's engine mode.
    let operand = store_v42(&te, 5, AccountMode::ReadOnly);
    let recipient = store_v42(&te, 0, AccountMode::Delegated);
    let operand_before = load_v42_lamports(&te, operand).expect("operand exists");
    let recipient_before = load_v42_lamports(&te, recipient).expect("recipient exists");
    assert_eq!(
        te.execute(&[transfer(operand, recipient, 1)]).await,
        Err(TransactionError::InvalidWritableAccount)
    );
    assert_eq!(
        load_v42_lamports(&te, operand).expect("operand remains"),
        operand_before,
        "immutable source debit discarded"
    );
    assert_eq!(
        load_v42_lamports(&te, recipient).expect("recipient remains"),
        recipient_before,
        "recipient credit rolled back with the transaction"
    );

    // The immutable account is the fee payer itself. The harness `execute` always
    // pays with the engine authority, so this branch needs a hand-signed
    // transaction: message compilation merges the signer and the writable output
    // into account 0. Fees are zero and this SVM does no fee-payer validation, so
    // a program-owned payer loads as-is.
    let payer = Keypair::new();
    let acc = v42_builder(5, AccountMode::ReadOnly).build();
    te.accounts().store(&[(payer.pubkey(), acc)]).unwrap();
    let (_sig, view) = signed_view(&te, Some(&payer), E::lit(9).compose(payer.pubkey(), &[]));
    let result = te.transaction(view).unwrap().execute().await.unwrap();
    assert_eq!(result, Err(TransactionError::InvalidAccountForFee));
    assert_eq!(
        load_v42_data(&te, payer.pubkey()),
        Some(5),
        "fee-payer write discarded"
    );

    // Positive control: a delegated (mutable) account commits normally.
    let mutable = store_v42(&te, 0, AccountMode::Delegated);
    assert!(te.execute(&[E::lit(9).compose(mutable, &[])]).await.is_ok());
    assert_eq!(
        load_v42_data(&te, mutable),
        Some(9),
        "mutable write commits"
    );

    te.close().await;
}

// A protection flag rejects deletion even when the lifecycle mode could
// otherwise close. The unflagged account is the positive control.
#[tokio::test(flavor = "multi_thread")]
async fn protected_account_deletion_is_rejected() {
    let te = TestEngine::new().await;
    let protected = store_v42(&te, 5, AccountMode::Ephemeral);
    protect(&te, protected).await;
    let error = te
        .account(protected)
        .delete()
        .await
        .expect_err("protected deletion is rejected");
    assert!(matches!(
        error,
        EngineError::TransactionExecution(TransactionError::InstructionError(
            0,
            InstructionError::PrivilegeEscalation
        ))
    ));
    assert_eq!(load_v42_data(&te, protected), Some(5));

    let unprotected = store_v42(&te, 5, AccountMode::Ephemeral);
    te.account(unprotected).delete().await.unwrap();
    assert!(te.get_account(unprotected).is_none());

    te.close().await;
}

// Post-finalize actions are invoked via CPI after an account is created, and
// MagicRoot's `post_finalize` refuses to run them against a writable account that
// is not mutable. Creating a ReadOnly account with an attached v42 write is
// therefore rejected, and the whole creation rolls back — a distinct enforcement
// path from `validate_access` (this fires inside the program, not after).
#[tokio::test(flavor = "multi_thread")]
async fn post_finalize_immutable_action_is_rejected() {
    let te = TestEngine::new().await;

    let key = Pubkey::new_unique();
    let mut ixs = compose_v42_replacement(key, AccountMode::ReadOnly, 1);
    let post_finalize_idx = ixs.len();
    let post_finalize = MagicRootInstruction::PostFinalize(vec![E::lit(9).compose(key, &[])]);
    ixs.push(post_finalize.compose(key).unwrap());
    assert_eq!(
        te.execute(ixs.as_slice()).await,
        Err(TransactionError::InstructionError(
            post_finalize_idx as u8,
            InstructionError::Immutable
        )),
        "MagicRoot's PostFinalize guard rejects the immutable writable account"
    );
    assert!(
        te.get_account(key).is_none(),
        "the rejected creation commits nothing"
    );

    te.close().await;
}

// Privilege cannot be laundered through account creation: a single transaction
// that mixes MagicRoot's create-a-ReadOnly-account instructions with a top-level
// (foreign) v42 write is not privileged — `is_privileged` requires *every*
// instruction to be MagicRoot — so the guard runs and the whole transaction,
// creation included, reverts.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_foreign_write_on_created_readonly_is_rejected() {
    let te = TestEngine::new().await;

    let key = Pubkey::new_unique();
    // A missing account starts as ReadOnly at slot zero. Advance the replacement
    // slot so this test reaches the access guard rather than MagicRoot's
    // duplicate-replacement guard.
    let mut ixs = compose_v42_replacement(key, AccountMode::ReadOnly, 1);
    // The foreign instruction that makes the whole transaction non-privileged.
    ixs.push(E::lit(9).compose(key, &[]));

    assert_eq!(
        te.execute(ixs.as_slice()).await,
        Err(TransactionError::InvalidWritableAccount)
    );
    assert!(
        te.get_account(key).is_none(),
        "the mixed transaction reverts wholesale"
    );

    te.close().await;
}
