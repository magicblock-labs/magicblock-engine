//! Account CRUD through the MagicRoot builtin — the privileged mutation path
//! exposed by `AccountAccessor`. This path is untested below the engine: it needs
//! the always-on MagicRoot builtin plus the executor's per-thread authority
//! (MagicRoot authorizes the transaction's fee payer against it). Asserts the
//! materialize/delete round-trip and the sponsor-balance invariant, and
//! that post-finalize actions actually run.
#![cfg(test)]

use engine::{Engine, EngineError, PostFinalize, testkit::TestEngine};
use keeper::testkit::{
    V42_ID, load_v42_data, load_v42_lamports, patterned_bytes, store_v42, v42_builder,
};
use magic_root_interface::MagicRootInstruction;
use solana_account::{
    AccountBuilder, AccountMode, AccountSharedData, ReadableAccount, testkit::delegated_account,
};
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
fn account(owner: Pubkey, data: Vec<u8>, mode: AccountMode, slot: u64) -> AccountBuilder {
    AccountBuilder::default()
        .lamports(LAMPORTS)
        .owner(owner)
        .mode(mode)
        .slot(slot)
        .data(data)
}

/// Delegated account with `data` at `slot`.
fn delegated(owner: Pubkey, data: Vec<u8>, slot: u64) -> AccountBuilder {
    account(owner, data, AccountMode::Delegated, slot)
}

/// Materializes `mode`, entering transient through its required delegated state.
async fn materialize_with(engine: &Engine, key: Pubkey, owner: Pubkey, mode: AccountMode) {
    let initial = if mode == AccountMode::Transient {
        delegated(owner, vec![1], SLOT - 1)
    } else {
        account(owner, vec![1], mode, SLOT)
    };
    engine
        .account(key)
        .await
        .materialize(initial, None)
        .await
        .expect("initial account is created");
    if mode == AccountMode::Transient {
        engine
            .account(key)
            .await
            .materialize(account(owner, vec![1], mode, SLOT), None)
            .await
            .expect("delegated account enters transient");
    }
}

/// Asserts MagicRoot rejected the lifecycle patch in a complete-account sequence.
fn assert_invalid_lifecycle(error: EngineError) {
    let errored = matches!(
        error,
        EngineError::TransactionExecution(TransactionError::InstructionError(
            1,
            InstructionError::InvalidArgument
        ))
    );
    assert!(errored, "unexpected replacement error: {error:?}");
}

// The full lifecycle. `materialize` patches every non-flag field, balances
// lamport patches against the authority for a fresh account, and finalizes its
// flags. Re-materialization overwrites the same key; `delete` closes it.
// Mutations here keep the balance constant after initial materialization.
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

    te.account(key).await.materialize(created, None).await.unwrap();

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

    // Re-materialization overwrites the account in place: same-length data (the
    // patch sequence replaces the exact data length) and identical lamports.
    // Read-only accounts remain replaceable after finalization.
    te.account(key)
        .await
        .materialize(account(owner, vec![5; 16], AccountMode::ReadOnly, 11), None)
        .await
        .unwrap();
    let acc = te.get_account(key).expect("still exists");
    assert_eq!(acc.data(), &[5; 16], "materialization replaced the data");
    assert_eq!(acc.owner(), &owner, "materialization replaced the owner");
    assert!(acc.is(AccountMode::ReadOnly));

    // delete: the account is gone from storage.
    te.account(key).await.delete().await.unwrap();
    assert!(te.get_account(key).is_none(), "deleted account is removed");

    // The same operation also materializes a fresh account without actions.
    let key2 = Pubkey::new_unique();
    te.account(key2)
        .await
        .materialize(delegated(owner, vec![3; 8], 10), None)
        .await
        .unwrap();
    assert_eq!(te.get_account(key2).expect("materialized").data(), &[3; 8]);

    te.close().await;
}

// Account cloning reconstructs every field and data chunk in one atomic private
// transaction. Growing the same clone through the 64 KiB boundary and beyond,
// then shrinking it below the boundary and to empty, proves replacement keeps
// the exact data length. The caller supplies each successive current state.
#[tokio::test(flavor = "multi_thread")]
async fn account_clone_materialization_accepts_large_data() {
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

        te.account(key).await.materialize(account, None).await.unwrap();

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
async fn account_materialization_accepts_max_data_with_post_finalize() {
    const CPI_CALLS: usize = 257;

    let te = TestEngine::new().await;
    let key = Pubkey::new_unique();
    let source = store_v42(&te, 7, AccountMode::Delegated);
    let output = store_v42(&te, 0, AccountMode::Ephemeral);
    let data = patterned_bytes(MAX_PERMITTED_DATA_LENGTH as usize, 42);
    let account = delegated_account(
        Rent::default().minimum_balance(data.len()),
        data.clone(),
        Pubkey::new_unique(),
    )
    .slot(SLOT);
    let action = transfer(source, output, 1);

    let post = PostFinalize {
        source_program: V42_ID,
        actions: vec![action],
    };
    te.account(key)
        .await
        .materialize(account, Some(post))
        .await
        .expect("maximum-sized account and post-finalize action execute atomically");

    let stored = te.get_account(key).expect("maximum-sized account exists");
    assert_eq!(stored.data().len(), MAX_PERMITTED_DATA_LENGTH as usize);
    assert!(stored.data() == data, "maximum-sized account data differs");
    assert_eq!(load_v42_data(&te, source), Some(6));
    assert_eq!(load_v42_data(&te, output), Some(1));

    let failed_key = Pubkey::new_unique();
    let failed_account = delegated_account(
        Rent::default().minimum_balance(data.len()),
        data,
        Pubkey::new_unique(),
    )
    .slot(SLOT);
    let excessive_cpis = (1..CPI_CALLS)
        .fold(E::lit(1).cpi(), |expr, _| expr + E::lit(1).cpi())
        .compose(output, &[]);
    let post = PostFinalize {
        source_program: V42_ID,
        actions: vec![transfer(source, output, 1), excessive_cpis],
    };
    let error = te
        .account(failed_key)
        .await
        .materialize(failed_account, Some(post))
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

/// Proves program-cache entries follow the complete v42 account lifecycle:
/// transaction-local deletion hides a loaded program immediately but rolls back
/// on a later instruction failure, while committed deletion evicts the shared
/// entry so invalid executable data restored at the same key cannot use stale code.
#[tokio::test(flavor = "multi_thread")]
async fn account_program_cache_tracks_v42_lifecycle() {
    let te = TestEngine::new().await;
    let seeded = te.get_account(V42_ID).expect("v42 program is seeded");
    let program = Pubkey::new_unique();
    let closeable = AccountBuilder::from(seeded.clone())
        .mode(AccountMode::Ephemeral)
        .slot(seeded.slot() + 1);
    te.account(program).await.materialize(closeable, None).await.unwrap();

    let output = Pubkey::new_unique();
    te.accounts()
        .store(&[(
            output,
            v42_builder(0, AccountMode::Ephemeral).owner(program).build(),
        )])
        .unwrap();
    let invoke = |value| {
        let mut instruction = E::lit(value).compose(output, &[]);
        instruction.program_id = program;
        instruction.accounts.last_mut().unwrap().pubkey = program;
        instruction
    };

    te.execute(&[invoke(42)])
        .await
        .expect("fresh v42 program executes and primes the shared cache");
    assert_eq!(load_v42_data(&te, output), Some(42));

    let delete = MagicRootInstruction::Delete.compose(program).unwrap();
    assert_eq!(
        te.execute(&[delete, invoke(7)]).await,
        Err(TransactionError::InstructionError(
            1,
            InstructionError::UnsupportedProgramId
        )),
        "deletion hides the program from later instructions in the transaction"
    );
    assert!(
        te.get_account(program).is_some(),
        "failed transaction rolls back account deletion"
    );

    te.execute(&[invoke(7)])
        .await
        .expect("rolled-back deletion preserves the shared cache entry");
    assert_eq!(load_v42_data(&te, output), Some(7));

    te.account(program).await.delete().await.unwrap();
    assert!(
        te.get_account(program).is_none(),
        "committed deletion removes the account"
    );

    let invalid = AccountBuilder::from(seeded).mode(AccountMode::Ephemeral).data(vec![0]);
    te.accounts().store(&[(program, invalid.build())]).unwrap();
    assert_eq!(
        te.execute(&[invoke(9)]).await,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::UnsupportedProgramId
        )),
        "invalid restored executable cannot run through a stale compiled entry"
    );

    te.close().await;
}

/// Proves accepted replacements install the full image, while forbidden mode
/// and slot pairs roll back funding and never execute their follow-up actions.
#[tokio::test(flavor = "multi_thread")]
async fn account_replacement_slot_ordering() {
    let te = TestEngine::new().await;
    let owner = Pubkey::new_unique();

    for (from, to, slot) in [
        (AccountMode::ReadOnly, AccountMode::Delegated, SLOT),
        (AccountMode::Placeholder, AccountMode::Ephemeral, SLOT),
        (AccountMode::Transient, AccountMode::Delegated, SLOT + 1),
    ] {
        let key = Pubkey::new_unique();
        materialize_with(&te, key, owner, from).await;
        let replacement = account(owner, vec![2; 8], to, slot);
        te.account(key)
            .await
            .materialize(replacement.clone(), None)
            .await
            .expect("valid replacement commits without actions");
        assert_eq!(
            te.get_account(key).unwrap(),
            replacement.build::<AccountSharedData>()
        );
    }

    let output = store_v42(&te, 0, AccountMode::Ephemeral);
    for (from, to, slot) in [
        (AccountMode::Placeholder, AccountMode::Transient, SLOT),
        (AccountMode::Ephemeral, AccountMode::Delegated, SLOT),
        (AccountMode::System, AccountMode::ReadOnly, SLOT),
        (AccountMode::Delegated, AccountMode::Delegated, SLOT + 1),
        (AccountMode::Ephemeral, AccountMode::Ephemeral, SLOT + 1),
        (AccountMode::Transient, AccountMode::Transient, SLOT + 1),
        (AccountMode::ReadOnly, AccountMode::ReadOnly, SLOT),
        (AccountMode::ReadOnly, AccountMode::Delegated, SLOT - 1),
        (AccountMode::Transient, AccountMode::Delegated, SLOT),
        (AccountMode::Transient, AccountMode::Delegated, SLOT - 1),
    ] {
        let key = Pubkey::new_unique();
        // Seed the source directly so only replacement validation is under test.
        te.accounts()
            .store(&[(key, account(owner, vec![1], from, SLOT).build())])
            .unwrap();
        let state = || [key, output, te.authority()].map(|key| te.get_account(key));
        let before = state();
        let post = PostFinalize {
            source_program: V42_ID,
            actions: vec![E::lit(1).compose(output, &[])],
        };
        let error = te
            .account(key)
            .await
            .materialize(
                account(owner, vec![2; 8], to, slot).lamports(LAMPORTS + 100),
                Some(post),
            )
            .await
            .expect_err("forbidden mode or slot pair is rejected");
        assert_invalid_lifecycle(error);
        // The lamport patch precedes lifecycle validation: compare target,
        // action output, and sponsor to catch partial funding or action effects.
        assert_eq!(state(), before, "{from:?} -> {to:?} at {slot}");
    }

    te.close().await;
}

/// Proves creation and redelegation atomically roll back an earlier action on
/// failure, permit retry after reacquiring, and cannot replay actions once active.
#[tokio::test(flavor = "multi_thread")]
async fn account_activation_is_atomic() {
    let te = TestEngine::new().await;

    for redelegation in [false, true] {
        let key = Pubkey::new_unique();
        if redelegation {
            materialize_with(&te, key, Pubkey::new_unique(), AccountMode::Transient).await;
        }
        let output = store_v42(&te, 0, AccountMode::Ephemeral);
        let state = || [key, output, te.authority()].map(|key| te.get_account(key));
        let before = state();
        let replacement =
            delegated(V42_ID, 10_i64.to_le_bytes().to_vec(), SLOT + 1).lamports(LAMPORTS + 100);
        let actions = || PostFinalize {
            source_program: V42_ID,
            actions: vec![transfer(key, output, 1)],
        };
        let accessor = te.account(key).await;
        let mut failing = actions();
        failing.actions.push((E::lit(i64::MIN) - E::lit(1)).compose(output, &[]));
        let error = accessor
            .materialize(replacement.clone(), Some(failing))
            .await
            .expect_err("second action aborts the whole activation");
        // V42's stable Arithmetic error is 6: a different action failure must
        // not pass as evidence that the intended overflow was reached.
        assert!(
            matches!(
                error,
                EngineError::TransactionExecution(TransactionError::InstructionError(
                    _,
                    InstructionError::Custom(6)
                ))
            ),
            "expected arithmetic overflow, got {error:?}"
        );
        assert_eq!(
            state(),
            before,
            "replacement and earlier transfer roll back"
        );

        let accessor = te.account(key).await;
        assert_eq!(
            accessor.read(Clone::clone).unwrap(),
            before[0],
            "reacquired state still permits creation or redelegation"
        );
        accessor
            .materialize(replacement.clone(), Some(actions()))
            .await
            .expect("retry commits after reacquiring and rereading");

        let expected = replacement
            .clone()
            .data(9_i64.to_le_bytes().to_vec())
            .lamports(LAMPORTS + 99)
            .build::<AccountSharedData>();
        let funding = LAMPORTS + 100 - before[0].as_ref().map_or(0, |account| account.lamports());
        assert_eq!(te.get_account(key).unwrap(), expected);
        assert_eq!(load_v42_data(&te, output), Some(1));
        assert_eq!(
            load_v42_lamports(&te, output),
            Some(before[1].as_ref().unwrap().lamports() + 1)
        );
        assert_eq!(
            te.get_account(te.authority()).unwrap().lamports(),
            before[2].as_ref().unwrap().lamports() - funding
        );

        let activated = state();
        for slot in [SLOT + 1, SLOT + 2] {
            // Distinct payloads avoid signature deduplication masking lifecycle
            // rejection under the same recent blockhash.
            let duplicate = replacement.clone().slot(slot).lamports(LAMPORTS + 200);
            let error = te
                .account(key)
                .await
                .materialize(duplicate, Some(actions()))
                .await
                .expect_err("active delegation cannot be rematerialized");
            assert_invalid_lifecycle(error);
            assert_eq!(
                state(),
                activated,
                "duplicate activation cannot replay its transfer"
            );
        }
    }

    te.close().await;
}
