//! Full-stack, byte-exact replication invariants over real engines and loopback TCP.
//!
//! Followers must match the leader's cursor and seal checksums, not just state.
//! Mutations are non-idempotent so duplicate application is observable.

#![cfg(test)]

use std::{
    net::{Ipv4Addr, SocketAddr, TcpListener},
    sync::Arc,
    time::Duration,
};

use engine::testkit::{Pacing, TestEngine};
use keeper::testkit::{
    Dirs, WireVersion, keeper_builder, load_v42_data, patterned_bytes, sign_versioned_instructions,
    v42_builder, v42_padded_value,
};
use ledger::request::{BlockDetails, BlockParams, BlockResponse};
use nucleus::{KB, Slot, config::Authority, ledger::BlockstorePosition, shutdown::ShutdownManager};
use replicator::{ReplicationClient, ReplicationDispatcher};
use solana_account::{AccountBuilder, AccountMode, ReadableAccount};
use solana_keypair::{Keypair, Signer};
use solana_pubkey::Pubkey;
use solana_sysvar::rent::Rent;
use tokio::{sync::broadcast, time};
use v42_calculator_interface::builder::Expr as E;

/// Bound for every asynchronous replication assertion.
const TIMEOUT: Duration = Duration::from_secs(4);

type AccountSeed = (Pubkey, i64, AccountMode);

/// Returns an unused loopback address, released immediately after discovery.
fn loopback_addr() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.local_addr().unwrap()
}

/// Truncates one sealed superblock and waits for its storage cleanup.
fn truncate(ledger: &ledger::LedgerHandle) {
    if let Some(worker) = ledger.truncate().unwrap() {
        worker.join().expect("truncation worker panicked").unwrap();
    }
}

/// Starts an engine over throwaway directories seeded with `accounts`.
async fn engine(authority: Authority, accounts: &[AccountSeed], pacing: Pacing) -> TestEngine {
    let dirs = Dirs::default();
    let mut builder = keeper_builder(&dirs);
    builder.authority = authority;
    for &(key, value, mode) in accounts {
        builder.accounts.insert(key, v42_builder(value, mode).build());
    }
    TestEngine::from_builder(dirs, builder, pacing).await
}

/// Starts a leader and follower with distinct local identities and direct trust.
async fn engines(
    leader_accounts: &[AccountSeed],
    follower_accounts: &[AccountSeed],
) -> (TestEngine, TestEngine) {
    let leader_authority: Authority = Keypair::new().into();
    let follower_authority = Authority {
        local: Arc::new(Keypair::new()),
        remote: Some(leader_authority.local.pubkey()),
    };
    let leader = engine(leader_authority, leader_accounts, Pacing::External).await;
    let follower = engine(follower_authority, follower_accounts, Pacing::External).await;
    (leader, follower)
}

/// Starts a leader-side dispatcher at an already selected address.
async fn dispatcher(addr: SocketAddr, leader: &TestEngine, allowed: &[Pubkey]) -> ShutdownManager {
    let mut shutdown = ShutdownManager::default();
    let engine = (*leader).clone();
    ReplicationDispatcher::spawn(addr, engine, Arc::from(allowed), &mut shutdown)
        .await
        .unwrap();
    shutdown
}

/// Registers a replication client with the follower lifecycle manager.
fn replicate(addr: SocketAddr, follower: &mut TestEngine) {
    let engine = follower.clone();
    let pacer = follower.pacer();
    ReplicationClient::spawn(addr, engine, pacer, follower.shutdown()).unwrap();
}

/// Subscribes before starting replication so the first published position cannot be missed.
fn stream(addr: SocketAddr, follower: &mut TestEngine) -> broadcast::Receiver<BlockstorePosition> {
    let positions = follower.ledger().position.subscribe();
    replicate(addr, follower);
    positions
}

/// Stages a snapshot through replication and installs it on restart.
async fn restart_from_snapshot(addr: SocketAddr, mut follower: TestEngine) -> TestEngine {
    replicate(addr, &mut follower);
    follower.shutdown().wait().await;
    let (dirs, authority) = follower.close().await;
    TestEngine::with(dirs, authority).await
}

/// Closes a follower after its producer publishes the boundary and Ingest-stop heartbeat.
async fn close_follower(follower: TestEngine, producer: &mut TestEngine) -> (Dirs, Authority) {
    let mut close = Box::pin(follower.close());
    assert!(
        time::timeout(Duration::from_millis(100), close.as_mut()).await.is_err(),
        "follower waits for a replicated block boundary"
    );
    producer.advance(2).await;
    producer.sync().await;
    time::timeout(TIMEOUT, close)
        .await
        .expect("follower drains through the producer boundary in time")
}

/// Applies a non-idempotent mutation so duplicate replication changes the result.
/// Calls must be separated by a block advance to produce distinct signatures.
async fn increment(engine: &TestEngine, state: Pubkey) {
    let ix = (E::acc(1) + E::lit(1)).compose(state, &[state]);
    engine.execute(&[ix]).await.expect("increment commits");
}

/// Commits one increment and publishes its enclosing block cursor.
async fn commit_increment(engine: &mut TestEngine, state: Pubkey) -> BlockstorePosition {
    increment(engine, state).await;
    engine.advance(1).await;
    engine.sync().await
}

/// Waits for the exact cursor, flushes the follower, and verifies state and position.
async fn await_replication(
    positions: &mut broadcast::Receiver<BlockstorePosition>,
    follower: &TestEngine,
    expected: BlockstorePosition,
    state: Pubkey,
    value: i64,
) {
    await_position(positions, expected).await;
    follower.sync().await;
    assert_eq!(load_v42_data(follower, state), Some(value));
    assert_eq!(follower.superblocks().position(), expected);
}

/// Waits until a follower publishes exactly the expected durable cursor.
async fn await_position(
    positions: &mut broadcast::Receiver<BlockstorePosition>,
    expected: BlockstorePosition,
) {
    time::timeout(TIMEOUT, async {
        loop {
            let observed = positions.recv().await.expect("position stream is open");
            if observed < expected {
                continue;
            }
            assert_eq!(observed, expected, "follower advanced past the leader");
            return;
        }
    })
    .await
    .expect("replication reaches the synced cursor in time");
}

/// Loads the serialized transactions committed in `slot`, preserving ledger order.
async fn block_transactions(engine: &TestEngine, slot: Slot) -> Vec<Vec<u8>> {
    let response = engine
        .blocks()
        .get(BlockParams {
            slot,
            details: BlockDetails::Transactions,
        })
        .await
        .expect("block read succeeds")
        .expect("committed block exists");
    let BlockResponse::WithTransactions(block) = response else {
        panic!("transaction detail request returns transactions");
    };
    block.transactions
}

/// Large raw transaction frames survive both retained catch-up and live replay.
#[tokio::test(flavor = "multi_thread")]
async fn replays_large_transactions_during_catch_up_and_live_streaming() {
    const BATCHED_INSTRUCTIONS: usize = 32;
    const BATCHED_TERMS: usize = 16;
    const CREATE_DATA_LEN: usize = 64 * KB + 1;
    const UPDATE_DATA_LEN: usize = 128 * KB + 1;
    const ACCOUNT_SLOT: Slot = 42;

    let state = Pubkey::new_unique();
    let seed = [(state, 0, AccountMode::Delegated)];
    let (mut leader, mut follower) = engines(&seed, &seed).await;
    let account = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let lamports = Rent::default().minimum_balance(UPDATE_DATA_LEN);

    // Persist a standard client transaction and a large private create before
    // connecting, forcing both frames through retained-ledger catch-up.
    let instructions: Vec<_> = (0..BATCHED_INSTRUCTIONS)
        .map(|value| v42_padded_value(state, value as i64, BATCHED_TERMS))
        .collect();
    let (_, client_transaction) = sign_versioned_instructions(
        leader.signer(),
        WireVersion::Legacy,
        &instructions,
        leader.blockhash(),
    );
    let client_transaction_len = client_transaction.len();
    assert!(client_transaction_len > 4 * KB);
    assert!(client_transaction_len < u16::MAX as usize);
    leader
        .execute(client_transaction)
        .await
        .expect("large client transaction commits");

    let created = AccountBuilder::default()
        .lamports(lamports)
        .owner(owner)
        .mode(AccountMode::ReadOnly)
        .slot(ACCOUNT_SLOT)
        .data(patterned_bytes(CREATE_DATA_LEN, 1));
    leader
        .account(account)
        .await
        .materialize(created, None)
        .await
        .expect("large account creation commits");

    let catch_up_slot = leader.blocks().current_slot();
    leader.advance(1).await;
    let expected = leader.sync().await;
    let transactions = block_transactions(&leader, catch_up_slot).await;
    assert_eq!(transactions.len(), 2);
    assert_eq!(transactions[0].len(), client_transaction_len);
    assert!(transactions[1].len() > 64 * KB);

    let addr = loopback_addr();
    let follower_identity = follower.signer().pubkey();
    let mut dispatcher = dispatcher(addr, &leader, &[follower_identity]).await;
    let mut positions = stream(addr, &mut follower);
    await_replication(
        &mut positions,
        &follower,
        expected,
        state,
        (BATCHED_INSTRUCTIONS - 1) as i64,
    )
    .await;
    assert_eq!(follower.get_account(account), leader.get_account(account));

    // Grow and replace the same account while connected, exercising the live
    // tail with a different multi-chunk payload and advancing account slot.
    let updated = AccountBuilder::default()
        .lamports(lamports)
        .owner(owner)
        .mode(AccountMode::ReadOnly)
        .slot(ACCOUNT_SLOT + 1)
        .data(patterned_bytes(UPDATE_DATA_LEN, 2));
    leader
        .account(account)
        .await
        .materialize(updated, None)
        .await
        .expect("large account update commits");

    let live_slot = leader.blocks().current_slot();
    leader.advance(1).await;
    let expected = leader.sync().await;
    let transactions = block_transactions(&leader, live_slot).await;
    assert_eq!(transactions.len(), 1);
    assert!(transactions[0].len() > 64 * KB);

    await_replication(
        &mut positions,
        &follower,
        expected,
        state,
        (BATCHED_INSTRUCTIONS - 1) as i64,
    )
    .await;
    assert_eq!(follower.get_account(account), leader.get_account(account));

    close_follower(follower, &mut leader).await;
    dispatcher.terminate().await;
    leader.close().await;
}

/// Proves a 128-plus-one catch-up batch preserves transaction order and its block fence.
#[tokio::test(flavor = "multi_thread")]
async fn batches_transactions_without_crossing_block_boundaries() {
    const CATCH_UP_TRANSACTIONS: i64 = 129;

    let state = Pubkey::new_unique();
    let seed = [(state, 0, AccountMode::Delegated)];
    let (mut leader, mut follower) = engines(&seed, &seed).await;

    // Distinct assignments make any cross-batch reordering observable in final state.
    for value in 1..=CATCH_UP_TRANSACTIONS {
        let instruction = E::lit(value).compose(state, &[]);
        leader.schedule(&[instruction]).await;
    }
    let catch_up_slot = leader.blocks().current_slot();
    leader.advance(1).await;
    let expected = leader.sync().await;
    let expected_transactions = block_transactions(&leader, catch_up_slot).await;
    assert_eq!(
        expected_transactions.len(),
        CATCH_UP_TRANSACTIONS as usize,
        "the stream crosses the transaction-count batch limit exactly once"
    );

    let addr = loopback_addr();
    let follower_identity = follower.signer().pubkey();
    let mut dispatcher = dispatcher(addr, &leader, &[follower_identity]).await;
    let mut positions = stream(addr, &mut follower);
    await_replication(
        &mut positions,
        &follower,
        expected,
        state,
        CATCH_UP_TRANSACTIONS,
    )
    .await;
    assert_eq!(
        block_transactions(&follower, catch_up_slot).await,
        expected_transactions,
        "the 128-plus-one split retains byte-exact ledger order"
    );

    // A following live transaction must remain behind the catch-up block fence.
    let live_value = CATCH_UP_TRANSACTIONS + 1;
    let instruction = E::lit(live_value).compose(state, &[]);
    leader.schedule(&[instruction]).await;
    let live_slot = leader.blocks().current_slot();
    leader.advance(1).await;
    let expected = leader.sync().await;
    await_replication(&mut positions, &follower, expected, state, live_value).await;
    assert_eq!(
        block_transactions(&follower, live_slot).await,
        block_transactions(&leader, live_slot).await,
        "the live tail starts in the block after the catch-up fence"
    );
    assert_eq!(
        follower.ledger().transactions(),
        leader.ledger().transactions()
    );

    close_follower(follower, &mut leader).await;
    dispatcher.terminate().await;
    leader.close().await;
}

/// An internally paced leader advances a follower whose delegated state survives restart.
#[tokio::test(flavor = "multi_thread")]
async fn internally_paced_replication_persists_across_restart() {
    let state = Pubkey::new_unique();
    let volatile = Pubkey::new_unique();
    let seed = [(state, 0, AccountMode::Delegated), (volatile, 7, AccountMode::ReadOnly)];
    let leader_authority: Authority = Keypair::new().into();
    let follower_authority = Authority {
        local: Arc::new(Keypair::new()),
        remote: Some(leader_authority.local.pubkey()),
    };
    let leader = engine(leader_authority, &seed, Pacing::Internal).await;
    let mut follower = engine(follower_authority, &seed, Pacing::External).await;

    let addr = loopback_addr();
    let initial = follower.superblocks().position();
    let follower_identity = follower.signer().pubkey();
    let mut dispatcher = dispatcher(addr, &leader, &[follower_identity]).await;
    let mut positions = stream(addr, &mut follower);
    let observed = time::timeout(TIMEOUT, async {
        loop {
            let observed = positions.recv().await.expect("position stream is open");
            if observed > initial {
                return observed;
            }
        }
    })
    .await
    .expect("follower observes an internally paced block in time");

    let (dirs, follower_authority) = follower.close().await;
    let follower = TestEngine::with(dirs, follower_authority).await;
    assert!(follower.get_account(volatile).is_none());
    assert_eq!(load_v42_data(&follower, state), Some(0));
    assert!(follower.superblocks().position() >= observed);

    dispatcher.terminate().await;
    follower.close().await;
    leader.close().await;
}

/// Catches up across sealed superblocks, resumes from the durable cursor without
/// re-applying, and drops volatile state on reset.
#[tokio::test(flavor = "multi_thread")]
async fn streams_and_resumes_without_duplicate_application() {
    let state = Pubkey::new_unique();
    // Only reset may remove the read-only account; delegated state must survive it.
    let volatile = Pubkey::new_unique();
    let seed = [(state, 0, AccountMode::Delegated), (volatile, 7, AccountMode::ReadOnly)];
    let (mut leader, mut follower) = engines(&seed, &seed).await;

    // Catch-up crosses two sealed superblocks and a live tail from byte zero.
    for _ in 0..2 {
        increment(&leader, state).await;
        leader.seal_and_archive().await;
    }
    let expected = commit_increment(&mut leader, state).await;

    let upstream = loopback_addr();
    let follower_identity = follower.signer().pubkey();

    // A valid follower signature is still rejected until its local identity is allowed.
    let initial = follower.superblocks().position();
    let mut denied_dispatcher = dispatcher(upstream, &leader, &[]).await;
    let mut rejected = ShutdownManager::default();
    ReplicationClient::spawn(upstream, follower.clone(), follower.pacer(), &mut rejected).unwrap();
    time::timeout(TIMEOUT, rejected.wait())
        .await
        .expect("denied replication client terminates in time");
    assert_eq!(follower.superblocks().position(), initial);
    rejected.terminate().await;
    denied_dispatcher.terminate().await;

    let mut first_dispatcher = dispatcher(upstream, &leader, &[follower_identity]).await;
    let mut positions = stream(upstream, &mut follower);

    // Replay must reproduce the leader's seals and block height, not just state.
    await_replication(&mut positions, &follower, expected, state, 3).await;
    assert_eq!(
        follower.superblocks().sealed(),
        leader.superblocks().sealed()
    );
    assert_eq!(
        follower.blocks().latest().slot,
        leader.blocks().latest().slot
    );

    let expected = commit_increment(&mut leader, state).await;
    await_replication(&mut positions, &follower, expected, state, 4).await;

    // Resume after an outage from the durable cursor. Exactly-once application
    // yields 5; applying the same entry twice would yield 6.
    first_dispatcher.terminate().await;
    let expected = commit_increment(&mut leader, state).await;
    let mut second_dispatcher = dispatcher(upstream, &leader, &[follower_identity]).await;
    await_replication(&mut positions, &follower, expected, state, 5).await;

    // Account creation debits the sponsor on both nodes before reset replenishes it.
    let authority = leader.authority();
    let authority_before = leader.get_account(authority).expect("leader sponsor exists").lamports();
    let sponsored = Pubkey::new_unique();
    leader
        .account(sponsored)
        .await
        .materialize(v42_builder(0, AccountMode::Delegated), None)
        .await
        .expect("sponsored account creation commits");
    let authority_after = leader.get_account(authority).expect("leader sponsor remains").lamports();
    assert!(
        authority_after < authority_before,
        "account creation debits the leader sponsor"
    );
    leader.advance(1).await;
    let expected = leader.sync().await;
    await_replication(&mut positions, &follower, expected, state, 5).await;
    assert_eq!(
        follower.get_account(authority).expect("follower sponsor remains").lamports(),
        authority_after,
        "sponsor debit replicates to the follower"
    );

    // Reset discards volatile accounts, retains delegated state, and replenishes the sponsor.
    leader.reset(99).expect("leader reset records");
    assert_eq!(
        leader
            .get_account(authority)
            .expect("leader sponsor exists after reset")
            .lamports(),
        authority_before,
        "reset replenishes the leader sponsor"
    );
    let expected = leader.sync().await;
    await_replication(&mut positions, &follower, expected, state, 5).await;
    assert!(follower.get_account(volatile).is_none());
    assert_eq!(
        follower
            .get_account(authority)
            .expect("follower sponsor exists after reset")
            .lamports(),
        authority_before,
        "reset replenishes the follower sponsor"
    );

    close_follower(follower, &mut leader).await;
    second_dispatcher.terminate().await;
    leader.close().await;
}

/// Models a real leader restart end to end: the connection drops, the engine
/// goes down and comes back from durable state, and the still-active follower
/// reconnects and resumes byte-exactly. An update committed just before the
/// restart is recovered, and a further update produced after it streams live —
/// each applied exactly once (the non-idempotent increment would overshoot on a
/// duplicate).
#[tokio::test(flavor = "multi_thread")]
async fn resumes_after_leader_restart() {
    let state = Pubkey::new_unique();
    let seed = [(state, 0, AccountMode::Delegated)];
    let (mut leader, mut follower) = engines(&seed, &seed).await;

    let addr = loopback_addr();
    let follower_identity = follower.signer().pubkey();
    let mut first_dispatcher = dispatcher(addr, &leader, &[follower_identity]).await;
    let mut positions = stream(addr, &mut follower);

    // Bring the follower current before the leader goes down.
    let expected = commit_increment(&mut leader, state).await;
    await_replication(&mut positions, &follower, expected, state, 1).await;

    // Take the dispatcher down, then commit an update the follower cannot see.
    first_dispatcher.terminate().await;
    let expected = commit_increment(&mut leader, state).await;

    // Restart the leader; the reopened engine must durably reload the update.
    let (dirs, authority) = leader.close().await;
    let mut leader = TestEngine::with(dirs, authority).await;
    assert_eq!(
        load_v42_data(&leader, state),
        Some(2),
        "reopened leader durably reloaded the update"
    );

    // Replication resumes on the same address; the still-active follower
    // reconnects from its durable cursor and recovers the pre-restart update.
    let mut second_dispatcher = dispatcher(addr, &leader, &[follower_identity]).await;
    await_replication(&mut positions, &follower, expected, state, 2).await;

    // The reopened leader keeps producing: a new update streams live over the
    // resumed connection, proving block production continues from the durable
    // slot rather than restarting and stalling the follower.
    let expected = commit_increment(&mut leader, state).await;
    await_replication(&mut positions, &follower, expected, state, 3).await;

    close_follower(follower, &mut leader).await;
    second_dispatcher.terminate().await;
    leader.close().await;
}

/// Proves shutdown reconnects, drains to a durable boundary, and reopens from it.
#[tokio::test(flavor = "multi_thread")]
async fn graceful_shutdown_drains_to_next_block_and_reopens() {
    let (mut leader, mut follower) = engines(&[], &[]).await;
    let addr = loopback_addr();
    let follower_identity = follower.signer().pubkey();
    let mut first_dispatcher = dispatcher(addr, &leader, &[follower_identity]).await;
    let mut positions = stream(addr, &mut follower);
    leader.advance(1).await;
    let expected = leader.sync().await;
    await_position(&mut positions, expected).await;

    let mut shutdown = Box::pin(follower.shutdown().terminate());
    assert!(
        time::timeout(Duration::from_millis(100), shutdown.as_mut()).await.is_err(),
        "shutdown waits while no replicated block boundary exists"
    );

    first_dispatcher.terminate().await;
    let mut second_dispatcher = dispatcher(addr, &leader, &[follower_identity]).await;
    leader.advance(1).await;
    let expected = leader.sync().await;
    // The next operational heartbeat lets Ingest observe the closed handoff
    // without out-of-band socket interruption.
    leader.advance(1).await;
    leader.sync().await;
    time::timeout(TIMEOUT, shutdown.as_mut())
        .await
        .expect("shutdown completes after the next leader block");
    drop(shutdown);
    assert_eq!(follower.superblocks().position(), expected);

    let (dirs, authority) = follower.close().await;
    let mut follower = TestEngine::with(dirs, authority).await;
    assert_eq!(follower.superblocks().position(), expected);

    let mut positions = stream(addr, &mut follower);
    leader.advance(1).await;
    let expected = leader.sync().await;
    await_position(&mut positions, expected).await;

    close_follower(follower, &mut leader).await;
    second_dispatcher.terminate().await;
    leader.close().await;
}

/// Installs the newest retained snapshot on restart, then streams every durable
/// ledger entry committed after that snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn restores_the_newest_snapshot_then_streams_its_tail() {
    let state = Pubkey::new_unique();
    let seed = [(state, 0, AccountMode::Delegated)];
    // The empty follower can acquire `state` only through snapshot restoration.
    let (mut leader, follower) = engines(&seed, &[]).await;
    let set = |value| E::lit(value).compose(state, &[]);

    // Distinct values distinguish the newest snapshot from its streamed tail.
    for value in [10, 20] {
        leader.execute(&[set(value)]).await.unwrap();
        leader.seal_and_archive().await;
    }
    leader.execute(&[set(30)]).await.unwrap();
    leader.advance(1).await;
    let expected = leader.sync().await;
    // Make the newest snapshot the only possible handshake response.
    truncate(leader.ledger()); // superblock 0
    truncate(leader.ledger()); // superblock 1
    assert!(
        leader.ledger().cursor(0).is_none(),
        "follower cursor was retained away"
    );
    assert!(
        leader.ledger().cursor(1).is_none(),
        "older snapshot history was retained away"
    );

    let addr = loopback_addr();
    let follower_identity = follower.signer().pubkey();
    let mut dispatcher = dispatcher(addr, &leader, &[follower_identity]).await;
    let mut follower = restart_from_snapshot(addr, follower).await;
    assert_eq!(
        load_v42_data(&follower, state),
        Some(20),
        "restart restores the newest completed snapshot, not the post-snapshot tail"
    );

    let mut positions = stream(addr, &mut follower);
    await_replication(&mut positions, &follower, expected, state, 30).await;

    close_follower(follower, &mut leader).await;
    dispatcher.terminate().await;
    leader.close().await;
}

/// Replicated state is itself replicable: a follower seals and archives from
/// replicated blocks alone, and its reconstruction serves a further follower
/// both as a live stream and as a snapshot bootstrap.
#[tokio::test(flavor = "multi_thread")]
async fn cascades_replication_through_a_follower() {
    let state = Pubkey::new_unique();
    let seed = [(state, 0, AccountMode::Delegated)];
    let shared = Arc::new(Keypair::new());
    let leader_authority: Authority = shared.clone().into();
    let middle_authority = Authority {
        local: shared.clone(),
        remote: Some(shared.pubkey()),
    };
    let tail_authority = Authority {
        local: Arc::new(Keypair::new()),
        remote: Some(shared.pubkey()),
    };
    let mut leader = engine(leader_authority, &seed, Pacing::External).await;
    let mut middle = engine(middle_authority, &seed, Pacing::External).await;
    let tail = engine(tail_authority, &[], Pacing::External).await;

    let leader_addr = loopback_addr();
    let middle_addr = loopback_addr();
    let middle_identity = middle.signer().pubkey();
    let tail_identity = tail.signer().pubkey();
    let mut leader_dispatcher = dispatcher(leader_addr, &leader, &[middle_identity]).await;
    let mut middle_positions = stream(leader_addr, &mut middle);
    let mut middle_dispatcher = dispatcher(middle_addr, &middle, &[tail_identity]).await;

    // Subscribe before the boundary because this seal is driven by replication,
    // outside the testkit's race-free seal-and-archive helper.
    let mut archives = middle.accounts().subscribe_snapshots();

    // Cross the seal live rather than during handshake catch-up.
    increment(&leader, state).await;
    leader.seal_and_archive().await;
    let expected = leader.sync().await;
    await_replication(&mut middle_positions, &middle, expected, state, 1).await;
    assert_eq!(
        middle.superblocks().sealed(),
        leader.superblocks().sealed(),
        "middle sealed the replicated boundary to the leader's checksum"
    );
    time::timeout(TIMEOUT, archives.recv())
        .await
        .expect("middle archives its replicated seal in time")
        .unwrap();

    // Keep the archive behind live state so restore and streaming are distinguishable.
    let expected = commit_increment(&mut leader, state).await;
    await_replication(&mut middle_positions, &middle, expected, state, 2).await;

    // The successor archive must become the only answer to the tail's cursor.
    truncate(middle.ledger()); // superblock 0
    assert!(
        middle.ledger().cursor(0).is_none(),
        "tail cursor was retained away"
    );

    let mut tail = restart_from_snapshot(middle_addr, tail).await;
    assert_eq!(
        load_v42_data(&tail, state),
        Some(1),
        "tail restores the middle's own archive, not the state streamed past it"
    );

    let mut tail_positions = stream(middle_addr, &mut tail);
    await_replication(&mut tail_positions, &tail, expected, state, 2).await;

    // Both hops cross the next seal live.
    increment(&leader, state).await;
    leader.seal_and_archive().await;
    let expected = leader.sync().await;
    await_replication(&mut middle_positions, &middle, expected, state, 3).await;
    await_replication(&mut tail_positions, &tail, expected, state, 3).await;
    assert_eq!(middle.superblocks().sealed(), leader.superblocks().sealed());
    assert_eq!(tail.superblocks().sealed(), leader.superblocks().sealed());

    close_follower(tail, &mut leader).await;
    close_follower(middle, &mut leader).await;
    middle_dispatcher.terminate().await;
    leader_dispatcher.terminate().await;
    leader.close().await;
}
