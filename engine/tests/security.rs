//! Account-mutability enforcement at the engine boundary.
//!
//! Instruction mutation checks reject writes to immutable modes, including after
//! a lifecycle transition. The transaction-final guard separately accepts valid
//! lifecycle writeback. These black-box tests assert errors and atomic rollback,
//! including foreign actions invoked through privileged account creation.
#![cfg(test)]

use engine::{EngineError, testkit::TestEngine};
use keeper::testkit::{load_v42_data, load_v42_lamports, signed_view, store_v42, v42_builder};
use magic_root_interface::{MagicRootInstruction, PostFinalize};
use solana_account::{AccountFieldPatch, AccountMode, ReadableAccount};
use solana_instruction::Instruction;
use solana_instruction_error::InstructionError;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::TransactionError;
use v42_calculator_interface::builder::{Expr as E, transfer};

/// Proves lifecycle writeback succeeds, but later lamport credits roll back both
/// the transition and source debit; unchanged-balance transfers remain valid.
#[tokio::test(flavor = "multi_thread")]
async fn lifecycle_transition_blocks_later_credits() {
    let te = TestEngine::new().await;
    for (from, to) in [
        (AccountMode::Delegated, AccountMode::Transient),
        (AccountMode::Magic, AccountMode::Closed),
    ] {
        let target = store_v42(&te, 5, from);
        let before = te.get_account(target).unwrap();
        let source = te.authority();
        let source_before = te.get_account(source).unwrap().lamports();
        let transition = || {
            MagicRootInstruction::Patch(AccountFieldPatch::Lifecycle {
                mode: to,
                slot: before.slot(),
            })
            .compose(target)
            .unwrap()
        };
        let credit =
            |amount| solana_system_interface::instruction::transfer(&source, &target, amount);

        assert_eq!(
            te.execute(&[transition(), credit(1)]).await,
            Err(TransactionError::InstructionError(
                1,
                InstructionError::Immutable
            )),
        );
        assert_eq!(
            te.get_account(target).unwrap(),
            before,
            "failed credit rolls back the entire target state"
        );
        assert_eq!(te.get_account(source).unwrap().lamports(), source_before);

        te.execute(&[transition(), credit(0)]).await.unwrap();
        if to == AccountMode::Closed {
            assert!(te.get_account(target).is_none());
        } else {
            let after = te.get_account(target).unwrap();
            assert_eq!(after.mode(), to);
            assert_eq!(after.lamports(), before.lamports());
        }
    }
    te.close().await;
}

/// Complete v42 account replacement at an explicit non-default slot.
fn compose_v42_replacement(key: Pubkey, mode: AccountMode, slot: u64) -> Vec<Instruction> {
    let account = v42_builder(0, mode).slot(slot).build();
    MagicRootInstruction::compose_account(key, account).unwrap()
}

/// Proves immutable operands and fee payers fail before commit, while delegated
/// accounts remain writable.
#[tokio::test(flavor = "multi_thread")]
async fn immutable_writes_are_rejected_and_not_committed() {
    let te = TestEngine::new().await;

    // The source's data mapping rejects the transfer before it can complete;
    // neither account's balance may change in storage.
    let operand = store_v42(&te, 5, AccountMode::ReadOnly);
    let recipient = store_v42(&te, 0, AccountMode::Delegated);
    let operand_before = load_v42_lamports(&te, operand).expect("operand exists");
    let recipient_before = load_v42_lamports(&te, recipient).expect("recipient exists");
    assert_eq!(
        te.execute(&[transfer(operand, recipient, 1)]).await,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::Immutable
        ))
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
    assert_eq!(
        result,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::Immutable
        ))
    );
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

/// Proves a foreign post-finalize action cannot bypass mode checks and its
/// failure rolls back the privileged creation as well.
#[tokio::test(flavor = "multi_thread")]
async fn post_finalize_immutable_action_is_rejected() {
    let te = TestEngine::new().await;

    let key = Pubkey::new_unique();
    let mut ixs = compose_v42_replacement(key, AccountMode::ReadOnly, 1);
    let post_finalize_idx = ixs.len();
    let post_finalize = MagicRootInstruction::PostFinalize(PostFinalize {
        source_program: Pubkey::new_unique(),
        actions: vec![E::lit(9).compose(key, &[])],
    });
    ixs.push(post_finalize.compose(key).unwrap());
    assert_eq!(
        te.execute(ixs.as_slice()).await,
        Err(TransactionError::InstructionError(
            post_finalize_idx as u8,
            InstructionError::Immutable
        )),
        "the post-finalize action cannot mutate an immutable account"
    );
    assert!(
        te.get_account(key).is_none(),
        "the rejected creation commits nothing"
    );

    te.close().await;
}

/// Proves PostFinalize rejects a recursive MagicRoot instruction of a delegated
/// account owned by an unrelated program and rolls back its creation.
#[tokio::test(flavor = "multi_thread")]
async fn post_finalize_rejects_magic_root_ix() {
    let te = TestEngine::new().await;

    let key = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let account = v42_builder(0, AccountMode::Delegated).owner(owner);
    let patch = MagicRootInstruction::Patch(AccountFieldPatch::DataAt {
        offset: 0,
        data: 9_i64.to_le_bytes().to_vec(),
    })
    .compose(key)
    .unwrap();

    let post = PostFinalize {
        source_program: owner,
        actions: vec![patch],
    };
    let error = te
        .account(key)
        .await
        .materialize(account, Some(post))
        .await
        .expect_err("PostFinalize rejects a recursive MagicRoot patch");
    assert!(
        matches!(
            error,
            EngineError::TransactionExecution(TransactionError::InstructionError(
                _,
                InstructionError::CallDepth
            ))
        ),
        "unexpected recursive invocation error: {error:?}"
    );
    assert!(
        te.get_account(key).is_none(),
        "the rejected recursive action rolls back account creation"
    );

    te.close().await;
}

/// Proves a top-level foreign write cannot inherit account-creation privileges;
/// the rejected mutation rolls back the preceding creation.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_foreign_write_on_created_readonly_is_rejected() {
    let te = TestEngine::new().await;

    let key = Pubkey::new_unique();
    // A missing account starts as ReadOnly at slot zero. Advance the replacement
    // slot so this test reaches the mutation check rather than MagicRoot's
    // duplicate-replacement guard.
    let mut ixs = compose_v42_replacement(key, AccountMode::ReadOnly, 1);
    // The foreign instruction that makes the whole transaction non-privileged.
    ixs.push(E::lit(9).compose(key, &[]));

    assert_eq!(
        te.execute(ixs.as_slice()).await,
        Err(TransactionError::InstructionError(
            (ixs.len() - 1) as u8,
            InstructionError::Immutable
        ))
    );
    assert!(
        te.get_account(key).is_none(),
        "the mixed transaction reverts wholesale"
    );

    te.close().await;
}
