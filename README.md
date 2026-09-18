<h1 align="center">MagicBlock Engine</h1>

<p align="center">
  <b>Execution engine for ephemeral rollups — Solana transactions over durable, locally-owned state.</b>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-Apache--2.0-6d4bd6" alt="License Apache-2.0">
  <img src="https://img.shields.io/badge/rust-1.96.1-orange?logo=rust" alt="Rust 1.96.1">
  <img src="https://img.shields.io/badge/edition-2024-blue" alt="Edition 2024">
  <img src="https://img.shields.io/badge/Solana-SVM-14F195?logo=solana&logoColor=white" alt="Solana SVM">
  <img src="https://img.shields.io/badge/status-experimental-yellow" alt="Status experimental">
  <img src="https://img.shields.io/badge/version-0.8.1-lightgrey" alt="Version 0.8.1">
</p>

---

MagicBlock Engine executes Solana transactions for ephemeral rollups. It owns
account state, records transaction and block history, and exposes asynchronous
APIs for execution, simulation, reads, and subscriptions.
It does not provide consensus, fork choice, or confirmation policy.

## 🚀 Starting the engine

The embedding service must retain both the engine and its `ShutdownManager`.
The manager coordinates every background service started by `Engine::new`.

```rust
use std::{path::PathBuf, time::Duration};

use engine::Engine;
use keeper::builder::KeeperBuilder;
use nucleus::{
    config::{AccountsDBParams, BlockstoreParams, LedgerParams},
    shutdown::ShutdownManager,
};
use solana_keypair::Keypair;
use solana_sysvar::rent::Rent;

async fn open_engine(
    home: PathBuf,
) -> engine::Result<(Engine, ShutdownManager)> {
    let mut shutdown = ShutdownManager::default();
    let builder = KeeperBuilder {
        authority: Keypair::new().into(),
        accountsdb: AccountsDBParams {
            directory: home.join("accountsdb"),
            lru_capacity: 10_000,
        },
        ledger: LedgerParams {
            directory: home.join("ledger"),
            size_limit: 256 * 1024 * 1024 * 1024,
        },
        blockstore: BlockstoreParams {
            blocktime: Duration::from_millis(400),
            superblock: 16,
        },
        builtins: Default::default(),
        programs: Default::default(),
        accounts: Default::default(),
        rent: Rent::default(),
    };

    let engine = Engine::new(builder, None, &mut shutdown).await?;
    Ok((engine, shutdown))
}
```

The second argument selects block pacing: `None` produces blocks locally; an
external channel supplies production or replay boundaries. `superblock = 0`
disables periodic sealing of nonzero slots; followers still apply upstream seals.

Internal pacing clears chain-mirrored volatile accounts at startup, preserving
internal system accounts. External pacing retains restored volatile state.

## 🛑 Shutdown

Retain the engine and its `ShutdownManager`. Stop external ingress before
terminating managed services:

```rust
let cause = shutdown.wait().await;
// Stop accepting transactions and other external work here.
let outcome = shutdown.terminate().await;
```

Inspect both reasons: `wait` identifies the trigger (including a replication
snapshot restart), while `terminate` reports failures encountered during draining.
The manager stops replication, pacing, execution, and backing services in order.
Dropping it cancels services but does not wait for them.

Internal pacing publishes a final block and flushes durable state. External
pacing flushes the ledger cursor before saving volatile state for restart. See
[shutdown details](engine/README.md#shutdown).

## 🔁 Replication

A leader serves retained history over TCP; followers replay it or bootstrap
from a snapshot. On the leader machine:

```rust
use std::sync::Arc;

use replicator::ReplicationDispatcher;

let allowed = Arc::from([follower_identity]);
ReplicationDispatcher::spawn(bind_addr, engine.clone(), allowed, &mut shutdown).await?;
```

On the follower machine, open its engine with an external pacer and connect the
client to the leader's address:

```rust
use replicator::ReplicationClient;
use tokio::sync::mpsc;

let (block_tx, block_rx) = mpsc::channel(16);
builder.authority.remote = Some(leader_identity);
let engine = Engine::new(builder, Some(block_rx), &mut shutdown).await?;
ReplicationClient::spawn(leader_addr, engine.clone(), block_tx, &mut shutdown)?;
```

The allowlist contains follower **local** identities; an empty list denies all
followers. Set the follower's remote authority to the source identity. Handshake
clocks must agree within 30 seconds. Distinct-key followers cannot serve replicas;
relays must hold the source's private key.

If history has expired, the client stages a snapshot and reports `RestartRequired`.
The host must drain services and reopen the engine from the same directories. See
[replication contracts](replicator/README.md) for authentication, relay, and
shutdown behavior.

## 📦 Account state

Delegated, ephemeral, and transient accounts are authoritative and persisted;
externally mirrored accounts are volatile. `Transient` remains persisted but is
not user-mutable. Accountsdb handles backend changes and removes closed accounts.

To replace accounts directly, acquire `Engine::account(pubkey).await`.
`materialize` and `delete` each run as one signed, committed transaction and
require the local signer to match the engine authority.

```rust
use solana_account::{AccountBuilder, AccountMode};
use solana_pubkey::Pubkey;

let key = Pubkey::new_unique();
let owner = Pubkey::new_unique();
let account = AccountBuilder::default()
    .lamports(2_000_000)
    .owner(owner)
    .mode(AccountMode::ReadOnly)
    .slot(1)
    .data(vec![1, 2, 3, 4])
    .build();

engine.account(key).await.materialize(account, None).await?;
engine.account(key).await.delete().await?;
```

`materialize` also replaces existing accounts and can run post-finalize actions
atomically. Lifecycle rules apply: a newer slot alone does not authorize replacing
engine-owned state. Mutations consume the accessor and retain its lease through
completion even if the caller stops waiting. Reacquire and reread before retrying;
a timeout or completion-task failure does not prove rollback. See the
[replacement and redelegation contract](engine/README.md#account-replacement).

Missing external accounts can be coordinated with `Engine::accounts().ensure`.
The first caller receives `MissingAccount::Load`; concurrent callers receive a
wait handle for the same pubkey. After storing the account, the loader calls
`AccountLoad::complete(mode)` to publish success and update recency tracking for
non-authoritative accounts. Dropping the load guard instead wakes waiters with a
failed outcome.

---

## 📨 Transactions

`Engine::transaction` accepts an instruction slice, `Message`, sanitized
`TransactionView`, or encoded transaction bytes. Instruction slices and messages
use the effective authority as payer and the local signer with the latest
blockhash, so local composition requires those identities to match.

```rust
use engine::Engine;
use solana_instruction::Instruction;

async fn submit(
    engine: &Engine,
    instructions: &[Instruction],
) -> engine::Result<()> {
    engine
        .transaction(instructions)?
        .execute()
        .await?
        .map_err(Into::into)
}
```

- `execute` waits for admission rejection or the committed result without an internal deadline.
- `schedule` acknowledges queueing only; admission rejection is silently dropped.
- `simulate` executes against owned account copies without committing state.

---

## 📡 Subscriptions

Keeper accessors expose dedicated Tokio channels for live state:

```rust
let mut account_updates = engine.accounts().subscribe(key).await;
let mut blocks = engine.blocks().subscribe();

let account = account_updates.recv().await.expect("account stream is open");
let block = blocks.recv().await.expect("block stream is open");
```

Related accessors subscribe to program-owned accounts, cache evictions,
snapshot completion, transaction status, logs, processed transactions, and
service messages. Signatures use terminal oneshot channels; other multicast
streams give each consumer a bounded queue and disconnect a consumer that falls
behind. Processed transactions, service messages, and cache evictions each have
one process-lifetime consumer and apply producer backpressure when its queue is
full.

---

## 🩹 Startup and recovery

Startup validates accountsdb against retained history, restores a retained
snapshot when necessary, and replays missing entries. Seal or final transaction
count mismatches fail startup with `ReplayError::StateMismatch`; recovery requires
a valid snapshot when current state is unusable. See
[recovery and restart semantics](engine/README.md#startup-and-recovery).

This is not a guarantee of complete history after a crash: ledger indexes become
visible asynchronously and no crash-tail index rebuild is performed. Graceful
shutdown is the supported complete-history boundary; see
[ledger durability](ledger/README.md#append-and-read-paths).

## 🧩 Workspace layout

| Crate | Role |
| :-- | :-- |
| [`nucleus`](nucleus/README.md) | Shared ledger, runtime, metrics, TLS, and shutdown types. |
| [`solana/*`](solana/README.md) | The runtime forks required by the engine account model. |
| [`accountsdb`](accountsdb/README.md) | Owns persisted and volatile account storage and snapshots. |
| [`ledger`](ledger/README.md) | Stores transactions, execution records, blocks, and superblocks. |
| [`keeper`](keeper/README.md) | Opens both stores and provides caches, reads, and subscriptions. |
| [`processor`](processor/README.md) | Schedules transactions across SVM executors and commits results. |
| `programs/*` | MagicRoot and the v42 test program and interfaces. |
| [`engine`](engine/README.md) | Wires the execution engine and exposes the public handle. |
| [`replicator`](replicator/README.md) | Streams durable engine state between nodes. |

Transactions are appended before execution, then paired with execution metadata.
Superblock boundaries quiesce execution for coherent account snapshots.
