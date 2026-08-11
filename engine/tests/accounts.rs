//! Account CRUD through the MagicRoot builtin — the privileged mutation path
//! exposed by `AccountAccessor`. This path is untested below the engine: it needs
//! the always-on MagicRoot builtin plus the executor's per-thread authority
//! (MagicRoot authorizes the transaction's fee payer against it). Asserts the
//! create/update/delete round-trip and the sponsor-balance invariant, and
//! that post-finalize actions actually run.
#![cfg(test)]

use engine::{Engine, EngineError, testkit::TestEngine};
use keeper::testkit::{
    V42_ID, load_v42_data, load_v42_lamports, patterned_bytes, store_v42, v42_builder,
};
use solana_account::{AccountBuilder, AccountMode, OwnedAccount, ReadableAccount};
use solana_instruction_error::InstructionError;
use solana_pubkey::Pubkey;
use solana_system_interface::MAX_PERMITTED_DATA_LENGTH;
use solana_sysvar::rent::Rent;
use solana_transaction::TransactionError;
use v42_calculator_interface::builder::{Expr as E, transfer};

/// Rent-exempt for the data sizes used below; the SVM rejects a created account
/// that falls under the rent floor.
const LAMPORTS: u64 = 2_000_000;
const SLOT: u64 = 42;

/// Account with explicit lifecycle state, funded at the shared rent-exempt balance.
fn account(owner: Pubkey, data: Vec<u8>, mode: AccountMode, slot: u64) -> OwnedAccount {
    AccountBuilder::default()
        .lamports(LAMPORTS)
        .owner(owner)
        .mode(mode)
        .slot(slot)
        .data(data)
        .build()
}

/// Delegated account with `data` at `slot`.
fn delegated(owner: Pubkey, data: Vec<u8>, slot: u64) -> OwnedAccount {
    account(owner, data, AccountMode::Delegated, slot)
}

/// Materializes `mode`, entering transient through its required delegated state.
async fn create_with(engine: &Engine, key: Pubkey, owner: Pubkey, mode: AccountMode) {
    let initial = if mode == AccountMode::Transient {
        delegated(owner, vec![1], SLOT - 1)
    } else {
        account(owner, vec![1], mode, SLOT)
    };
    engine
        .account(key)
        .create(initial, None)
        .await
        .expect("initial account is created");
    if mode == AccountMode::Transient {
        engine
            .account(key)
            .update(account(owner, vec![1], mode, SLOT))
            .await
            .expect("delegated account enters transient");
    }
}

/// Asserts MagicRoot rejected the slot patch in a complete-account sequence.
fn assert_non_advancing_slot(error: EngineError) {
    let errored = matches!(
        error,
        EngineError::TransactionExecution(TransactionError::InstructionError(
            2,
            InstructionError::InvalidArgument
        ))
    );
    assert!(errored, "unexpected replacement error: {error:?}");
}

/// Asserts MagicRoot rejected the mode patch in a complete-account sequence.
fn assert_invalid_mode_transition(error: EngineError) {
    let errored = matches!(
        error,
        EngineError::TransactionExecution(TransactionError::InstructionError(
            1,
            InstructionError::InvalidArgument
        ))
    );
    assert!(errored, "unexpected replacement error: {error:?}");
}

// The full lifecycle. `create` materializes a fresh account by patching every
// non-flag field, balancing lamport patches against the authority, then
// finalizing its flags; `update` overwrites an existing account or materializes
// a fresh key; and
// `delete` closes it. Mutations here keep the balance constant after creation.
#[tokio::test(flavor = "multi_thread")]
async fn account_crud_lifecycle() {
    let te = TestEngine::new().await;
    let key = Pubkey::new_unique();
    let owner = Pubkey::new_unique();

    let created = account(
        owner,
        vec![1, 2, 3, 4, 5, 6, 7, 8],
        AccountMode::ReadOnly,
        10,
    );
    let authority_before = te.get_account(te.authority()).expect("sponsor exists").lamports();

    te.account(key).create(created, None).await.unwrap();

    let acc = te.get_account(key).expect("created account exists");
    assert_eq!(acc.lamports(), LAMPORTS);
    assert_eq!(acc.owner(), &owner);
    assert_eq!(acc.data(), &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert!(acc.is(AccountMode::ReadOnly));

    let authority_after = te.get_account(te.authority()).expect("sponsor exists").lamports();
    assert_eq!(
        authority_before - authority_after,
        LAMPORTS,
        "the lamport patch sponsors the created balance from the authority"
    );

    // update overwrites the existing account in place: same-length data (the
    // patch sequence replaces the exact data length) and identical lamports.
    // Read-only accounts remain replaceable after finalization.
    te.account(key)
        .update(account(owner, vec![5; 16], AccountMode::ReadOnly, 11))
        .await
        .unwrap();
    let acc = te.get_account(key).expect("still exists");
    assert_eq!(acc.data(), &[5; 16], "update replaced the data wholesale");
    assert_eq!(acc.owner(), &owner, "update replaced the patched owner");
    assert!(acc.is(AccountMode::ReadOnly));

    // delete: the account is gone from storage.
    te.account(key).delete().await.unwrap();
    assert!(te.get_account(key).is_none(), "deleted account is removed");

    // update also materializes a fresh account the same way create does, minus
    // the post-finalize actions.
    let key2 = Pubkey::new_unique();
    te.account(key2).update(delegated(owner, vec![3; 8], 10)).await.unwrap();
    assert_eq!(te.get_account(key2).expect("materialized").data(), &[3; 8]);

    te.close().await;
}

// Account cloning reconstructs every field and data chunk in one atomic private
// transaction. Growing the same clone through the 64 KiB boundary and beyond,
// then shrinking it below the boundary and to empty, proves replacement keeps
// the exact data length. The caller supplies each successive current state.
#[tokio::test(flavor = "multi_thread")]
async fn account_clone_create_and_update_accept_large_data() {
    const MAX_DATA_LEN: usize = 128 * 1024 + 1;

    let te = TestEngine::new().await;
    let key = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let lamports = Rent::default().minimum_balance(MAX_DATA_LEN);

    for (index, (len, seed)) in [
        (u16::MAX as usize, 1),
        (64 * 1024, 2),
        (MAX_DATA_LEN, 3),
        (32 * 1024, 4),
        (0, 5),
    ]
    .into_iter()
    .enumerate()
    {
        let data = patterned_bytes(len, seed);
        let account = AccountBuilder::default()
            .lamports(lamports)
            .owner(owner)
            .mode(AccountMode::ReadOnly)
            .slot(SLOT + index as u64)
            .data(data.clone());

        if index == 0 {
            te.account(key).create(account, None).await.unwrap();
        } else {
            te.account(key).update(account).await.unwrap();
        }

        let stored = te.get_account(key).expect("large account exists");
        assert_eq!(stored.lamports(), lamports);
        assert_eq!(stored.owner(), &owner);
        assert!(stored.is(AccountMode::ReadOnly));
        assert_eq!(stored.slot(), SLOT + index as u64);
        assert_eq!(stored.data(), data);
    }

    te.close().await;
}

/// Proves an exact maximum-sized Solana account can run a PostFinalize SBPF
/// action above trace index 64, while 257 subsequent V42 self-CPIs hit the CPI
/// trace limit and roll back both the account creation and an earlier action.
#[tokio::test(flavor = "multi_thread")]
async fn account_create_accepts_max_data_with_post_finalize() {
    const CPI_CALLS: usize = 257;

    let te = TestEngine::new().await;
    let key = Pubkey::new_unique();
    let source = store_v42(&te, 7, AccountMode::Delegated);
    let output = store_v42(&te, 0, AccountMode::Ephemeral);
    let data = patterned_bytes(MAX_PERMITTED_DATA_LENGTH as usize, 42);
    let account = AccountBuilder::default()
        .lamports(Rent::default().minimum_balance(data.len()))
        .owner(Pubkey::new_unique())
        .mode(AccountMode::Delegated)
        .slot(SLOT)
        .data(data.clone());
    let action = transfer(source, output, 1);

    te.account(key)
        .create(account, Some(vec![action]))
        .await
        .expect("maximum-sized account and post-finalize action execute atomically");

    let stored = te.get_account(key).expect("maximum-sized account exists");
    assert_eq!(stored.data().len(), MAX_PERMITTED_DATA_LENGTH as usize);
    assert!(stored.data() == data, "maximum-sized account data differs");
    assert_eq!(load_v42_data(&te, source), Some(6));
    assert_eq!(load_v42_data(&te, output), Some(1));

    let failed_key = Pubkey::new_unique();
    let failed_account = AccountBuilder::default()
        .lamports(Rent::default().minimum_balance(data.len()))
        .owner(Pubkey::new_unique())
        .mode(AccountMode::Delegated)
        .slot(SLOT)
        .data(data);
    let excessive_cpis = (1..CPI_CALLS)
        .fold(E::lit(1).cpi(), |expr, _| expr + E::lit(1).cpi())
        .compose(output, &[]);
    let error = te
        .account(failed_key)
        .create(
            failed_account,
            Some(vec![transfer(source, output, 1), excessive_cpis]),
        )
        .await
        .expect_err("257 V42 self-CPIs exceed the trace limit");

    assert!(
        matches!(
            error,
            EngineError::TransactionExecution(TransactionError::InstructionError(
                _,
                InstructionError::MaxInstructionTraceLengthExceeded
            ))
        ),
        "unexpected CPI trace error: {error:?}"
    );
    assert!(
        te.get_account(failed_key).is_none(),
        "failed creation was rolled back"
    );
    assert_eq!(load_v42_data(&te, source), Some(6));
    assert_eq!(load_v42_data(&te, output), Some(1));

    te.close().await;
}

// Replacing the seeded executable at a newer slot exercises MagicRoot's large
// account reconstruction and executable finalization, then proves the resulting
// program remains usable by executing it through the normal client path.
#[tokio::test(flavor = "multi_thread")]
async fn account_update_replaces_v42_program_at_new_slot() {
    let te = TestEngine::new().await;
    let program = te.get_account(V42_ID).expect("v42 program is seeded");
    let data = program.data().to_vec();
    let owner = *program.owner();
    let lamports = program.lamports();
    let slot = program.slot() + 1;
    let replacement = AccountBuilder::from(program).slot(slot);

    te.account(V42_ID).update(replacement).await.unwrap();

    let replaced = te.get_account(V42_ID).expect("replaced v42 program exists");
    assert_eq!(replaced.lamports(), lamports);
    assert_eq!(replaced.owner(), &owner);
    assert_eq!(replaced.slot(), slot);
    assert!(replaced.executable());
    assert_eq!(replaced.data(), data);

    let output = store_v42(&te, 0, AccountMode::Ephemeral);
    te.execute(&[E::lit(42).compose(output, &[])])
        .await
        .expect("replaced v42 program executes");
    assert_eq!(load_v42_data(&te, output), Some(42));

    te.close().await;
}

// Complete replacements are monotonic by slot. An equal-slot replacement is
// meaningful only when it performs a real lifecycle transition; mode is patched
// before slot, and no-op mode writes deliberately leave the mode marker clean.
#[tokio::test(flavor = "multi_thread")]
async fn account_replacement_slot_ordering() {
    let te = TestEngine::new().await;
    let owner = Pubkey::new_unique();

    for (from, to) in [
        (AccountMode::ReadOnly, AccountMode::Delegated),
        (AccountMode::Placeholder, AccountMode::Ephemeral),
    ] {
        let key = Pubkey::new_unique();
        create_with(&te, key, owner, from).await;
        te.account(key)
            .update(account(owner, vec![2], to, SLOT))
            .await
            .expect("equal-slot mode transition is accepted");

        let updated = te.get_account(key).expect("transitioned account exists");
        assert!(updated.is(to), "{from:?} transitions to {to:?}");
        assert_eq!(updated.slot(), SLOT);
        assert_eq!(updated.data(), &[2]);
    }

    for (from, to) in [
        (AccountMode::Placeholder, AccountMode::Transient),
        (AccountMode::Ephemeral, AccountMode::Delegated),
        (AccountMode::System, AccountMode::ReadOnly),
    ] {
        let key = Pubkey::new_unique();
        // Seed the source directly so only the mode-transition invariant is
        // under test.
        te.accounts()
            .store(&[(key, account(owner, vec![1], from, SLOT).into())])
            .unwrap();
        let error = te
            .account(key)
            .update(account(owner, vec![2], to, SLOT))
            .await
            .expect_err("invalid mode transition is rejected");
        assert_invalid_mode_transition(error);

        let unchanged = te.get_account(key).expect("rejected transition preserves the account");
        assert!(unchanged.is(from), "{from:?} does not transition to {to:?}");
        assert_eq!(unchanged.slot(), SLOT);
        assert_eq!(unchanged.data(), &[1]);
    }

    let key = Pubkey::new_unique();
    te.account(key)
        .create(account(owner, vec![3], AccountMode::ReadOnly, SLOT), None)
        .await
        .expect("baseline account is created");

    let error = te
        .account(key)
        .update(account(owner, vec![4], AccountMode::ReadOnly, SLOT))
        .await
        .expect_err("equal-slot replacement without a mode change is rejected");
    assert_non_advancing_slot(error);

    let error = te
        .account(key)
        .update(account(owner, vec![5], AccountMode::Delegated, SLOT - 1))
        .await
        .expect_err("a mode change never authorizes an older slot");
    assert_non_advancing_slot(error);

    let unchanged = te.get_account(key).expect("rejected replacements preserve the account");
    assert!(unchanged.is(AccountMode::ReadOnly));
    assert_eq!(unchanged.slot(), SLOT);
    assert_eq!(unchanged.data(), &[3]);

    te.close().await;
}

// Post-finalize actions are invoked via CPI after the account is finalized, so a
// failing action aborts the whole creation (nothing commits), while a benign one
// lets it through. The contrast proves the actions actually execute rather than
// being silently dropped.
#[tokio::test(flavor = "multi_thread")]
async fn create_runs_post_finalize_actions() {
    let te = TestEngine::new().await;

    // A successful v42 transfer proves the post-finalize action ran after the
    // new account became writable and program-owned.
    let source = store_v42(&te, 0, AccountMode::Delegated);
    let source_before = load_v42_lamports(&te, source).expect("source exists");
    let ok_key = Pubkey::new_unique();
    let acc = v42_builder(0, AccountMode::Delegated);
    let benign = transfer(source, ok_key, 1);
    te.account(ok_key)
        .create(acc, Some(vec![benign]))
        .await
        .expect("create with a succeeding post-finalize action");
    assert_eq!(
        load_v42_lamports(&te, source).expect("source remains"),
        source_before - 1,
        "post-finalize action debited its source"
    );
    assert_eq!(
        load_v42_lamports(&te, ok_key).expect("created account exists"),
        source_before + 1,
        "post-finalize action credited the created account"
    );

    // An overflowing v42 action errors; the failure propagates and rolls back
    // the account creation in the same transaction.
    let bad_key = Pubkey::new_unique();
    let failing = (E::lit(i64::MIN) - E::lit(1)).compose(bad_key, &[]);
    let acc = v42_builder(0, AccountMode::Delegated);
    let result = te.account(bad_key).create(acc, Some(vec![failing])).await;
    assert!(
        result.is_err(),
        "failing post-finalize action surfaces an error"
    );
    assert!(
        te.get_account(bad_key).is_none(),
        "nothing commits when the action fails"
    );

    te.close().await;
}
