//! Transaction submission at the engine boundary: the `execute`, `simulate`, and
//! `schedule` wrappers around the sequencer. The processor suite already proves
//! the SVM commits/simulates correctly; these assert the `TransactionAccessor`
//! ergonomics on top — subscribe-then-await commit, the separate simulation
//! channel that never commits, and fire-and-forget scheduling.
#![cfg(test)]

use agave_transaction_view::MAX_STANDARD_TRANSACTION_SIZE;
use engine::testkit::TestEngine;
use keeper::testkit::{
    WireVersion, decode_v42, load_v42_data, load_v42_lamports, sign_versioned_instructions,
    signed_view, store_v42, v42_padded_value, v42_sum,
};
use nucleus::KB;
use solana_account::{AccountMode, ReadableAccount};
use solana_instruction_error::InstructionError;
use solana_packet::PACKET_DATA_SIZE;
use solana_transaction::TransactionError;
use v42_calculator_interface::builder::{Expr as E, transfer};

// The Engine accepts the same standard wire formats produced by Solana clients
// on either side of the canonical packet boundary. The wide form reads every
// supplied account, while the batched form independently exercises instruction
// framing instead of relying on one large payload.
#[tokio::test(flavor = "multi_thread")]
async fn client_transaction_formats_execute_below_and_above_packet_limit() {
    const WIDE_ACCOUNTS: usize = 32;
    const BATCHED_INSTRUCTIONS: usize = 32;
    const BATCHED_TERMS: usize = 16;
    const FOUR_KIB: usize = 4 * KB;

    let te = TestEngine::new().await;
    let operands: Vec<_> =
        (0..WIDE_ACCOUNTS).map(|_| store_v42(&te, 1, AccountMode::Delegated)).collect();

    for version in [WireVersion::Legacy, WireVersion::V0, WireVersion::V1] {
        let output = store_v42(&te, 0, AccountMode::Ephemeral);
        let (_, small) = sign_versioned_instructions(
            te.signer(),
            version,
            [E::lit(42).compose(output, &[])],
            te.blockhash(),
        );
        assert!(small.len() < PACKET_DATA_SIZE,);
        te.execute(small).await.expect("small v42 transaction succeeds");
        assert_eq!(load_v42_data(&te, output), Some(42));

        let output = store_v42(&te, 0, AccountMode::Ephemeral);
        let (_, wide) = sign_versioned_instructions(
            te.signer(),
            version,
            [v42_sum(output, &operands)],
            te.blockhash(),
        );
        assert!(wide.len() > PACKET_DATA_SIZE);
        assert!(wide.len() < MAX_STANDARD_TRANSACTION_SIZE);
        te.execute(wide).await.expect("wide v42 transaction succeeds");
        assert_eq!(load_v42_data(&te, output), Some(WIDE_ACCOUNTS as i64));

        let output = store_v42(&te, 0, AccountMode::Ephemeral);
        let instructions: Vec<_> = (0..BATCHED_INSTRUCTIONS)
            .map(|value| v42_padded_value(output, value as i64, BATCHED_TERMS))
            .collect();
        let (_, batched) =
            sign_versioned_instructions(te.signer(), version, &instructions, te.blockhash());
        assert!(batched.len() > FOUR_KIB);
        assert!(batched.len() < MAX_STANDARD_TRANSACTION_SIZE);
        te.execute(batched).await.expect("batched v42 transaction succeeds");
        assert_eq!(
            load_v42_data(&te, output),
            Some((BATCHED_INSTRUCTIONS - 1) as i64)
        );
    }

    te.close().await;
}

// Simulation runs against live state through the dedicated simulation channel
// but must not commit; execution of the same transfer does.
#[tokio::test(flavor = "multi_thread")]
async fn simulate_does_not_commit_execute_does() {
    let te = TestEngine::new().await;
    let source = store_v42(&te, 0, AccountMode::Delegated);
    let recipient = store_v42(&te, 0, AccountMode::Ephemeral);
    let source_before = load_v42_lamports(&te, source).expect("source exists");
    let recipient_before = load_v42_lamports(&te, recipient).expect("recipient exists");
    let ixs = [transfer(source, recipient, 42)];

    // The record's post-execution account copy proves simulation actually ran
    // the program, not merely that the channel round-tripped.
    let record = te.simulate(&ixs).await.expect("simulation resolves");
    let executed = record.result.expect("simulated transaction processes");
    assert!(executed.was_successful(), "simulated execution succeeds");
    let (_, simulated_source) = executed
        .loaded_transaction
        .accounts
        .iter()
        .find(|(key, _)| *key == source)
        .expect("simulation loaded the source account");
    assert_eq!(
        simulated_source.lamports(),
        source_before - 42,
        "simulation debited its source copy"
    );
    let (_, simulated_recipient) = executed
        .loaded_transaction
        .accounts
        .iter()
        .find(|(key, _)| *key == recipient)
        .expect("simulation loaded the recipient account");
    assert_eq!(
        simulated_recipient.lamports(),
        recipient_before + 42,
        "simulation credited its recipient copy"
    );
    assert_eq!(
        load_v42_lamports(&te, source).expect("source remains"),
        source_before,
        "simulation leaves the live source untouched"
    );
    assert_eq!(
        load_v42_lamports(&te, recipient).expect("recipient remains"),
        recipient_before,
        "simulation leaves the live recipient untouched"
    );

    assert!(te.execute(&ixs).await.is_ok(), "execution resolves");
    assert_eq!(
        load_v42_lamports(&te, source).expect("source remains"),
        source_before - 42,
        "execution commits the source debit"
    );
    assert_eq!(
        load_v42_lamports(&te, recipient).expect("recipient remains"),
        recipient_before + 42,
        "execution commits the recipient credit"
    );

    te.close().await;
}

// A real processed transaction retains its execution artifacts through keeper's
// projection and the ledger's compressed append→index→reader round-trip.
#[tokio::test(flavor = "multi_thread")]
async fn processed_transaction_details_roundtrip() {
    let te = TestEngine::new().await;
    let slot = te.blocks().current_slot();
    let mut expected = Vec::new();

    for value in [7, 42, 99] {
        let output = store_v42(&te, 0, AccountMode::Ephemeral);
        let ix = E::lit(value).cpi().compose(output, &[]);
        let (signature, transaction) = signed_view(&te, None, ix.clone());
        let bytes = transaction.inner_data().as_ref().clone();

        te.execute(&[ix]).await.expect("processed transaction succeeds");
        expected.push((signature, bytes, value));
    }

    te.sync().await;

    for (signature, bytes, value) in expected {
        let response = te
            .transactions()
            .get(signature)
            .await
            .expect("ledger read succeeds")
            .expect("processed transaction is retained");
        assert_eq!(response.transaction, bytes);
        assert_eq!(response.execution.header.signature, signature);
        assert_eq!(response.execution.header.slot, slot);
        assert!(response.execution.header.result.is_ok());

        let details = response.execution.details.expect("execution details retained");
        assert_eq!(
            details.fee, 0,
            "the engine does not charge transaction fees"
        );
        assert!(
            !details.balances.pre.is_empty(),
            "native balances were recorded"
        );
        assert_eq!(
            details.balances.pre, details.balances.post,
            "the calculator changes account data, not lamports"
        );
        assert!(details.logs.iter().any(|line| line.contains("v42:")));
        assert!(details.compute_units > 0);
        assert!(
            details
                .cpi
                .as_ref()
                .is_some_and(|groups| groups.iter().any(|group| !group.0.is_empty())),
            "the nested expression retains its CPI trace"
        );
        let returned = details.return_data.expect("CPI return data retained");
        assert_eq!(returned.program, v42_calculator_interface::ID.to_bytes());
        assert_eq!(returned.data.as_slice(), &value.to_le_bytes());
    }

    te.close().await;
}

// A transaction that runs but errors resolves as a committed error result and
// leaves its output account untouched — the engine surfaces the failure through
// the outer Ok / inner Err split rather than dropping it.
#[tokio::test(flavor = "multi_thread")]
async fn failed_execution_surfaces_error_result() {
    let te = TestEngine::new().await;
    let output = store_v42(&te, 5, AccountMode::Ephemeral);
    // MIN - 1 overflows the program's checked_sub before any write.
    let ixs = [(E::lit(i64::MIN) - E::lit(1)).compose(output, &[])];

    let error = te.execute(&ixs).await.expect_err("overflow yields an error result");
    // CalcError::Arithmetic = 6; its discriminants are stable for tests.
    assert_eq!(
        error,
        TransactionError::InstructionError(0, InstructionError::Custom(6)),
        "the program's own failure is surfaced, not a substitute"
    );
    assert_eq!(
        load_v42_data(&te, output),
        Some(5),
        "failed execution commits no writes"
    );

    te.close().await;
}

// schedule returns before the transaction commits; the write still lands, and an
// account subscription (not a poll loop) observes it.
#[tokio::test(flavor = "multi_thread")]
async fn schedule_is_fire_and_forget() {
    let te = TestEngine::new().await;
    let output = store_v42(&te, 0, AccountMode::Ephemeral);
    let mut updates = te.accounts().subscribe(output).await;
    let ixs = [E::lit(7).compose(output, &[])];

    te.schedule(&ixs).await;

    let account = updates.recv().await.expect("scheduled write reaches the subscriber");
    assert_eq!(
        decode_v42(&account),
        7,
        "scheduled transaction commits the write"
    );

    te.close().await;
}
