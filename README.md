<h1 align="center">MagicBlock Engine</h1>

<p align="center">
  <b>Execution engine for ephemeral rollups — Solana transactions over durable, locally-owned state.</b>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-Apache--2.0-6d4bd6" alt="License Apache-2.0">
  <img src="https://img.shields.io/badge/rust-1.96-orange?logo=rust" alt="Rust 1.96">
  <img src="https://img.shields.io/badge/edition-2024-blue" alt="Edition 2024">
  <img src="https://img.shields.io/badge/Solana-SVM-14F195?logo=solana&logoColor=white" alt="Solana SVM">
  <img src="https://img.shields.io/badge/status-experimental-yellow" alt="Status experimental">
  <img src="https://img.shields.io/badge/version-0.1.0-lightgrey" alt="Version 0.1.0">
</p>

---

MagicBlock Engine executes Solana transactions for ephemeral rollups. It owns
account state, records transaction and block history, and exposes asynchronous
APIs for execution, simulation, reads, and subscriptions.

`engine::Engine` is the consumer-facing handle. It combines durable state from
`keeper`, transaction scheduling from `processor`, block pacing, recovery, and
privileged account operations.

## ✨ Highlights

|   |   |   |
| :-- | :-- | :-- |
| ⚙️ **Runs Solana programs** — a real SVM, without the overhead of a validator | 🗃️ **Storage that fits the account** — engine-owned state on disk, chain-mirrored state in memory | 📚 **Full history** — transactions and blocks kept in segments you can retain or drop wholesale |
| 🔁 **Replication** — mirror a live engine onto standby nodes over TCP | 🩹 **Self-healing startup** — rebuilds and re-verifies its state from history after a crash | 📡 **Async APIs** — execute, simulate, read, and subscribe over live state |

## 📖 Contents

- [🚀 Starting the engine](#-starting-the-engine)
- [🛑 Shutdown](#-shutdown)
- [🔁 Replication](#-replication)
- [📦 Account state](#-account-state)
- [📨 Transactions](#-transactions)
- [📡 Subscriptions](#-subscriptions)
- [🩹 Startup and recovery](#-startup-and-recovery)
- [🧩 Workspace layout](#-workspace-layout)

---

## 🚀 Starting the engine

Bringing up an engine is mostly filling in one struct and awaiting one call —
everything underneath (storage, ledger, scheduler, background tasks) is wired up
for you.

The embedding service must retain both the engine and its `ShutdownManager`.
The manager coordinates every background service started by `Engine::new`.

```rust
use std::{num::NonZeroU64, path::PathBuf, time::Duration};

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
        },
        ledger: LedgerParams {
            directory: home.join("ledger"),
            size_limit: 256 * 1024 * 1024 * 1024,
        },
        blockstore: BlockstoreParams {
            blocktime: Duration::from_millis(400),
            superblock: NonZeroU64::new(16).unwrap(),
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

The second argument chooses who advances blocks. `None` runs the built-in
pacer, which produces blocks on its own clock — the standalone case. Passing a
channel instead makes block boundaries caller-driven, as replication followers
do when they step in time with a leader. External producers supply the slot and
timestamp; the sequencer computes and overwrites the block hash and parent.

The two modes also start differently: the built-in pacer wipes chain-mirrored
volatile accounts at startup (internal system accounts stay available), so a
standalone engine begins from clean external state. An external pacer keeps
whatever volatile state was restored, which replication depends on.

---

## 🛑 Shutdown

Shutdown isn't a hard stop — it unwinds in tiers, so in-flight work drains and
durable state lands on disk before the process goes away.

The host waits for an OS signal or premature service termination with
`ShutdownManager::wait`. It should then stop external ingress and call
`ShutdownManager::terminate` while retaining the engine handle.

```rust
let cause = shutdown.wait().await;

// Stop accepting transactions and other external work here.
shutdown.terminate().await;
```

`wait` returns whether shutdown was requested by an OS signal or by a managed
service terminating early. Embedding processes can use the service reason to
distinguish recoverable lifecycle events, such as a replication snapshot that
requires reopening the engine, from fatal failures.

Shutdown proceeds by service tier:

1. A replication client stops consuming upstream state.
2. The pacemaker stops producing boundaries and calls `Engine::shutdown`.
3. The already-drained sequencer and terminally-synced ledger appender stop.
4. Ledger readers, simulation, subscriptions, and other backing services stop.

Internal pacing publishes a final block and flushes durable state. External
pacing also writes volatile state to `CURRENT/volatile.db` after flushing the
corresponding ledger cursor. The final sync explicitly closes ledger workers,
so retained but inactive engine handles cannot hold shutdown open. Each tier
has a bounded termination window.

---

## 🔁 Replication

Point a follower at a leader and it keeps itself in sync — replaying the stream
when it can, and pulling a fresh snapshot when it has fallen too far behind.

Replication keeps a standby engine in step with a live one: a **leader** serves
its history over TCP, and one or more **followers** replay that stream to stay
current. On the leader machine, bind a dispatcher to a reachable address and
serve the retained ledger:

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

Leader and follower local keypairs do not need to match. The server allowlist
contains follower local identities and denies all access when empty. The
follower's remote authority identifies its immediate upstream, whose signed
responses must arrive within 30 seconds of the follower's clock.

The external pacer keeps replicated blocks ordered with transactions, resets,
and seals. If the leader's retained stream cannot satisfy the follower's cursor,
it sends the newest snapshot. The client stages it, reports `RestartRequired`
through the follower's shutdown manager, and the follower host reopens its
engine from the same directories.

---

## 📦 Account state

You never have to decide where an account lives — the engine watches what each
account *is* and keeps it in the right place on its own.

The engine holds two kinds of accounts and stores each where it makes sense:

- Accounts the engine controls — delegated, ephemeral, and transient — are
  authoritative here and **persisted to disk**.
- Accounts that only mirror external chain or system state — read-only,
  placeholders, and sysvars — are kept **in volatile memory**.

An account's `AccountMode::authoritative()` classification decides which side it
belongs to. When that changes, accountsdb moves the account and drops the stale
copy from the other backend, so there is only ever one live copy. `Transient`
accounts remain authoritative and persisted even though runtime code cannot
mutate them.

To change accounts directly, use `Engine::account(pubkey)`. `create`, `update`,
`patch`, and `delete` each run as one signed, committed transaction and require
the local signer to match the engine authority.

```rust
use solana_account::{AccountBuilder, AccountFieldPatch, AccountMode};
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

engine.account(key).create(account, None).await?;
let current = engine.accounts().get(&key)?;

engine
    .account(key)
    .patch(vec![AccountFieldPatch::DataAt {
        offset: 0,
        data: vec![9; 4],
    }])
    .await?;

let replacement = AccountBuilder::default()
    .lamports(2_000_000)
    .owner(owner)
    .mode(AccountMode::ReadOnly)
    .slot(2)
    .data(vec![5; 4])
    .build();
engine.account(key).update(replacement).await?;
engine.account(key).delete().await?;
```

Each mutation is one committed transaction. `create` can also run optional
post-finalize instructions in that transaction; if an instruction fails, the
creation does not commit. Complete-account patches cover non-flag fields, and
finalization atomically installs the caller-supplied flags without changing
lamports. Callers are responsible for supplying current state; later
replacements remain subject to the account's slot and lifecycle rules. Internal
create composition places post-finalize instructions immediately after
finalization.

Missing external accounts can be coordinated with `Engine::accounts().ensure`.
The first caller receives `MissingAccount::Load`; concurrent callers receive a
wait handle for the same pubkey. The loader stores the account and calls
`AccountLoad::commit` to publish the completed load.

---

## 📨 Transactions

Hand it whatever you've already got — a few instructions, a `Message`, or raw
encoded bytes — and pick how much you want to wait around for.

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

- `execute` waits for the committed transaction result.
- `schedule` queues execution without waiting for its result.
- `simulate` executes against owned account copies without committing state.

---

## 📡 Subscriptions

No polling loops — subscribe to what you care about and the engine pushes
updates as they happen.

Keeper accessors expose Tokio broadcast receivers for live state:

```rust
let mut account_updates = engine.accounts().subscribe(key).await;
let mut blocks = engine.blocks().subscribe();

let account = account_updates.recv().await?;
let block = blocks.recv().await?;
```

Related accessors subscribe to program-owned accounts, cache evictions,
snapshot completion, transaction status, logs, processed transactions, and
service messages. Broadcast consumers must handle `Lagged` when they fall
behind and `Closed` during shutdown; retained reads are available separately.

---

## 🩹 Startup and recovery

Crash it mid-write and it picks itself back up: on the next start it checks its
own state against history and rebuilds whatever doesn't line up.

Every startup reconciles the account store with the transaction history and
repairs the store when they disagree — so a crash, corruption, or a staged
replication snapshot all recover the same way.

Concretely: keeper validates the account store against the retained ledger. A
corrupt store, or a valid one whose latest checkpoint trails the ledger, is
replaced with the newest retained snapshot. If that restored state still trails
the ledger tip, the engine replays the missing history to catch up, checking the
rebuilt state against each recorded checkpoint and refusing to continue
(`ReplayError::StateMismatch`) if they diverge. When the store is already
current, nothing runs.

---

## 🧩 Workspace layout

| Crate | Role |
| :-- | :-- |
| `nucleus` | Shared ledger, runtime, metrics, TLS, and shutdown types. |
| `solana/*` | The runtime forks required by the engine account model. |
| `accountsdb` | Owns persisted and volatile account storage and snapshots. |
| `ledger` | Stores transactions, execution records, blocks, and superblocks. |
| `keeper` | Opens both stores and provides caches, reads, and subscriptions. |
| `processor` | Schedules transactions across SVM executors and commits results. |
| `programs/*` | MagicRoot and the v42 test program and interfaces. |
| `engine` | Wires the execution engine and exposes the public handle. |
| `replicator` | Streams durable engine state between nodes. |

Transactions are appended before execution, then paired with execution metadata.
Successful dirty accounts are written through accountsdb and live notifications
are published. Superblock boundaries quiesce execution while keeper snapshots
accountsdb and archives it beside the next retained ledger segment.

---

<p align="center">
  <sub>Built with 🦀 Rust · licensed under Apache-2.0 · © MagicBlock contributors</sub>
</p>
