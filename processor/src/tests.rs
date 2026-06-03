//! End-to-end processor checks driven by the loadable v42 calculator program.

use std::{sync::Arc, time::Duration};

use crate::{SequencerMessage, SimulatorMessage, sequencer::Sequencer};
use derive_more::Deref;
use keeper::{
    TransactionStatus, TransactionView,
    testkit::{TestKeeper, V42_ID, load_v42_data, load_v42_lamports, signed_view, store_v42},
};
use nucleus::{
    ledger::Block,
    runtime::{SequencerHandle, Simulation, barrier},
};
use solana_account::{AccountMode, ReadableAccount};
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_program_runtime::loaded_programs::ProgramCache;
use solana_pubkey::Pubkey;
use solana_sdk_ids::loader_v4;
use solana_signature::Signature;
use solana_svm::transaction_processing_result::{
    TransactionProcessingResult, TransactionProcessingResultExtensions,
};
use tokio::time::timeout;
use v42_calculator_interface::builder::{Expr as E, transfer};

/// End-to-end fixture with a real keeper, sequencer, simulator, and v42 program.
#[derive(Deref)]
struct Harness {
    /// Seeded keeper (v42 + funded payer) owning the engine state along with the
    /// directories and shutdown lifecycle its background services run on.
    #[deref]
    keeper: TestKeeper,
    /// Channels used by tests to drive execution and simulation paths.
    handle: SequencerHandle,
}

impl Harness {
    /// Builds a seeded keeper and wires a sequencer/simulator onto its lifecycle.
    ///
    /// `replay` selects whether the sequencer records transaction status while
    /// still committing account state, matching the processor replay mode.
    async fn new(replay: bool) -> Self {
        let (harness, sequencer) = Self::unspawned(replay).await;
        sequencer.spawn().unwrap();
        harness
    }

    /// Builds a seeded keeper and sequencer without starting the sequencer loop.
    ///
    /// This lets tests fill the execution channel before the sequencer can
    /// consume from it, forcing contention resolution to happen from a backlog.
    async fn unspawned(replay: bool) -> (Self, Sequencer) {
        let mut keeper = TestKeeper::new().await;
        let (sequencer, handle) = Sequencer::new(
            2,
            keeper.clone(),
            Arc::new(ProgramCache::default()),
            &mut keeper.shutdown,
            replay,
        )
        .unwrap();
        (Self { keeper, handle }, sequencer)
    }

    /// A signed transaction view paid for by a fresh payer, not the shared one.
    ///
    /// The shared payer is writable in every transaction it signs, which would
    /// make all of them conflict on that one account and mask the account
    /// intersections the contention tests exist to stress.
    fn fresh_payer_view(&self, instruction: Instruction) -> (Signature, TransactionView) {
        signed_view(self, Some(&Keypair::new()), instruction)
    }

    /// Queues a transaction on the execution path without waiting for commit.
    async fn execute(&self, tx: TransactionView) {
        self.handle.execution.send(SequencerMessage::Transaction(tx)).await.unwrap();
    }

    /// Runs a transaction through simulation and returns the raw processing result.
    ///
    /// Simulation shares the keeper state but must not persist account writes or
    /// transaction status.
    async fn simulate(&self, tx: TransactionView) -> TransactionProcessingResult {
        let (response, rx) = oneshot::channel();
        self.handle
            .simulation
            .send(SimulatorMessage::Transaction(Simulation {
                transaction: tx,
                response,
            }))
            .await
            .unwrap();
        rx.await.unwrap().expect("simulation resolves").result
    }

    /// Sends the same block transition to execution and simulation services.
    ///
    /// Both paths maintain sysvar caches, so block metadata must be delivered to
    /// both before comparing simulated and committed behavior.
    async fn set_block(&self, block: Block) {
        self.handle.execution.send(SequencerMessage::Block(block)).await.unwrap();
        self.handle.simulation.send(SimulatorMessage::Block(block)).await.unwrap();
    }

    /// Waits for all previously submitted execution work to finish, failing
    /// quickly if lock contention stops making progress.
    async fn barrier(&self) {
        let (controller, guard) = barrier();
        self.handle.execution.send(SequencerMessage::Barrier(guard)).await.unwrap();
        timeout(Duration::from_secs(8), controller.acknowledged)
            .await
            .expect("barrier timed out")
            .unwrap();
        controller.released.send(()).unwrap();
    }

    /// Returns the status recorded for `signature`, which must exist.
    async fn status(&self, signature: Signature) -> TransactionStatus {
        self.transactions()
            .status(signature)
            .await
            .unwrap()
            .expect("executed transaction records a status")
    }

    /// Drops service handles and waits for background tasks to stop.
    ///
    /// [`TestKeeper::close`] flushes, so nothing needs flushing here.
    async fn close(self) {
        let Self { keeper, handle } = self;
        drop(handle);
        keeper.close().await;
    }
}

/// Asserts that SVM processing accepted the transaction.
fn assert_success(result: &TransactionProcessingResult) {
    assert!(result.flattened_result().is_ok(), "{result:?}");
}

/// Wraps an expression in nested self-CPI calls.
fn with_cpi_depth(mut expr: E, depth: usize) -> E {
    for _ in 0..depth {
        expr = expr.cpi();
    }
    expr
}

/// Picks a read-only operand different from `output`.
fn operand(accounts: &[Pubkey], output: Pubkey, index: usize) -> Pubkey {
    let mut key = accounts[index % accounts.len()];
    if key == output {
        key = accounts[(index + 1) % accounts.len()];
    }
    key
}

// Simulation and execution both accept the same v42 transaction, but only
// execution commits account state and records transaction status.
#[tokio::test(flavor = "current_thread")]
async fn execution_commits_and_simulation_does_not() {
    let harness = Harness::new(false).await;
    let output = store_v42(&harness, 0, AccountMode::Delegated);
    let input = store_v42(&harness, 40, AccountMode::Delegated);
    let ix = (E::acc(1) + E::lit(2)).compose(output, &[input]);
    let (_, sim_tx) = signed_view(&harness, None, ix.clone());

    assert_success(&harness.simulate(sim_tx).await);
    assert_eq!(
        load_v42_data(&harness, output),
        Some(0),
        "simulation leaves state untouched"
    );

    let (signature, tx) = signed_view(&harness, None, ix);
    harness.execute(tx).await;
    harness.barrier().await;

    assert_eq!(load_v42_data(&harness, output), Some(42));
    harness.status(signature).await.result.expect("successful execution");
    harness.close().await;
}

/// Proves a finalized block hashes the canonical signature of an appended transaction.
#[tokio::test(flavor = "current_thread")]
async fn block_hash_includes_appended_transaction_signature() {
    let harness = Harness::new(false).await;
    let parent = harness.blockhash();
    let output = store_v42(&harness, 0, AccountMode::Delegated);
    let (signature, tx) = signed_view(&harness, None, E::lit(42).compose(output, &[]));
    let mut hasher = blake3::Hasher::new();
    hasher.update(parent.as_ref());
    hasher.update(signature.as_ref());
    let expected = Hash::from(*hasher.finalize().as_bytes());

    harness.execute(tx).await;
    harness.set_block(Block::new(1, 1234)).await;
    harness.barrier().await;

    assert_eq!(harness.blockhash(), expected);
    harness.status(signature).await.result.expect("successful execution");
    harness.close().await;
}

// Program seeding installs the v42 ELF under loader-v4, and recursive CPI
// preserves return-data flow through a committed execution.
#[tokio::test(flavor = "current_thread")]
async fn seeded_program_and_recursive_cpi_return_data_work() {
    let harness = Harness::new(false).await;
    let account = harness.accounts().loader().load(&V42_ID).unwrap().expect("v42 program seeded");

    assert!(account.executable());
    assert_eq!(*account.owner(), loader_v4::ID);
    assert_eq!(account.data().get(..4), Some(&[0x7f, b'E', b'L', b'F'][..]));

    let output = store_v42(&harness, 0, AccountMode::Delegated);
    let expr = E::lit(42) + (E::lit(31) * E::lit(4)).cpi() - E::lit(56);
    let (_, tx) = signed_view(&harness, None, expr.compose(output, &[]));

    harness.execute(tx).await;
    harness.barrier().await;

    assert_eq!(load_v42_data(&harness, output), Some(110));
    harness.close().await;
}

// A block transition updates the Clock sysvar for both simulation and execution,
// while committed execution lands in the next slot.
#[tokio::test(flavor = "current_thread")]
async fn block_transition_updates_execution_and_simulation_sysvars() {
    let harness = Harness::new(false).await;
    let block = Block::new(7, 1234);
    harness.set_block(block).await;

    let output = store_v42(&harness, 0, AccountMode::Delegated);
    let ix = E::clock().compose(output, &[]);
    let (_, sim_tx) = signed_view(&harness, None, ix.clone());
    assert_success(&harness.simulate(sim_tx).await);

    let (signature, tx) = signed_view(&harness, None, ix);
    harness.execute(tx).await;
    harness.barrier().await;

    assert_eq!(
        load_v42_data(&harness, output),
        Some(1234),
        "both paths see the block's clock"
    );
    assert_eq!(
        harness.status(signature).await.slot,
        8,
        "committed execution lands in the slot after the block"
    );
    harness.close().await;
}

// Replay mode still applies balance writes, but it must not publish transaction
// status because replayed entries were already recorded by the original run.
#[tokio::test(flavor = "current_thread")]
async fn replay_mode_commits_state_without_recording_status() {
    let harness = Harness::new(true).await;
    let source = store_v42(&harness, 0, AccountMode::Delegated);
    let recipient = store_v42(&harness, 0, AccountMode::Delegated);
    let source_before = load_v42_lamports(&harness, source).expect("source exists");
    let recipient_before = load_v42_lamports(&harness, recipient).expect("recipient exists");
    let (signature, tx) = signed_view(&harness, None, transfer(source, recipient, 1));

    harness.execute(tx).await;
    harness.barrier().await;

    assert_eq!(
        load_v42_lamports(&harness, source).expect("source remains"),
        source_before - 1,
        "replay commits the source debit"
    );
    assert_eq!(
        load_v42_lamports(&harness, recipient).expect("recipient remains"),
        recipient_before + 1,
        "replay commits the recipient credit"
    );
    assert!(
        harness.transactions().status(signature).await.unwrap().is_none(),
        "replay records no status"
    );
    harness.close().await;
}

// A transaction that runs but fails still commits a status receipt carrying the
// error, and leaves its output account untouched — the failure is recorded, not
// silently dropped like an unresolvable transaction.
#[tokio::test(flavor = "current_thread")]
async fn failed_execution_records_an_error_status() {
    let harness = Harness::new(false).await;
    let output = store_v42(&harness, 5, AccountMode::Delegated);
    // MIN - 1 overflows the program's checked_sub, so it returns an error before
    // ever writing the output account.
    let (signature, tx) = signed_view(
        &harness,
        None,
        (E::lit(i64::MIN) - E::lit(1)).compose(output, &[]),
    );

    harness.execute(tx).await;
    harness.barrier().await;

    // The failed run is committed as a status with an error result, unlike the
    // success cases the other tests assert.
    assert!(
        harness.status(signature).await.result.is_err(),
        "recorded result reflects the failure"
    );
    assert_eq!(
        load_v42_data(&harness, output),
        Some(5),
        "failed execution commits no writes"
    );
    harness.close().await;
}

// A backlog of writes to one account must keep making progress even when every
// transaction initially contends for the same lock.
#[tokio::test(flavor = "current_thread")]
async fn prefilled_same_writable_account_backlog_drains() {
    const TRANSACTIONS: usize = 128;

    let (harness, sequencer) = Harness::unspawned(false).await;
    let output = store_v42(&harness, 0, AccountMode::Delegated);
    let mut signatures = Vec::with_capacity(TRANSACTIONS);

    for i in 0..TRANSACTIONS {
        let expr = with_cpi_depth(E::lit((i + 1) as i64), i % 5);
        let (signature, tx) = harness.fresh_payer_view(expr.compose(output, &[]));
        signatures.push(signature);
        harness.execute(tx).await;
    }

    sequencer.spawn().unwrap();
    harness.barrier().await;

    assert_eq!(
        load_v42_data(&harness, output),
        Some(TRANSACTIONS as i64),
        "the last write of the drained backlog wins"
    );
    for signature in signatures {
        harness.status(signature).await.result.expect("successful execution");
    }
    harness.close().await;
}

// Mixed read-only and writable intersections should eventually resolve even
// when the sequencer starts with more conflicted work than it can keep unblocked
// at once.
#[tokio::test(flavor = "current_thread")]
async fn prefilled_mixed_read_write_contention_stress_drains() {
    const ACCOUNTS: usize = 16;
    const TRANSACTIONS: usize = 512;

    let (harness, sequencer) = Harness::unspawned(false).await;
    let accounts: Vec<_> = (0..ACCOUNTS)
        .map(|i| store_v42(&harness, i as i64 + 1, AccountMode::Delegated))
        .collect();
    let mut signatures = Vec::with_capacity(TRANSACTIONS);

    for i in 0..TRANSACTIONS {
        let output = match i % 4 {
            0 => accounts[0],
            1 => accounts[i % ACCOUNTS],
            2 => accounts[(i + 3) % ACCOUNTS],
            _ => accounts[(i * 7 + 5) % ACCOUNTS],
        };
        let left = operand(&accounts, output, i + 1);
        let right = operand(&accounts, output, i * 3 + 2);
        let expr = with_cpi_depth(E::acc(1) + E::lit((i % 5) as i64), i % 5);
        let (signature, tx) = harness.fresh_payer_view(expr.compose(output, &[left, right]));
        signatures.push(signature);
        harness.execute(tx).await;
    }

    sequencer.spawn().unwrap();
    harness.barrier().await;

    for signature in signatures {
        harness.status(signature).await.result.expect("successful execution");
    }
    harness.close().await;
}
