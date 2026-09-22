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
  <img src="https://img.shields.io/badge/version-0.11.0-lightgrey" alt="Version 0.11.0">
</p>

---

MagicBlock Engine brings Solana program execution into your Rust service. It
combines the SVM with persistent account state, transaction history, live
subscriptions, and replication: the execution foundation for ephemeral rollups.

Your application chooses what to execute and when. Engine runs the transactions
and keeps track of the resulting state. It is an embeddable library, not a
validator; consensus, fork choice, and confirmation policy remain with the host.

## ✨ Engine in practice

The sketches below show how the features fit together. They omit imports,
configuration details, and error handling to keep the focus on the workflow;
they are not standalone, compilable examples.

### 🚀 Bring execution into your service

Choose your programs, initial accounts, storage location, and block cadence.
Engine opens the stores, handles recovery, and starts execution behind one async
handle. Use its own clock for a standalone deployment, or supply block boundaries
from your application.

```rust
engine = Engine::new(config, pacing, shutdown).await;
```

Keep the engine and its shutdown manager alive while serving requests. Reuse the
same storage directories and authority identity when reopening a deployment.
See [startup configuration](keeper/README.md#startup-and-recovery).

### ⚡ Execute in parallel, or simulate first

Submit instructions or an already signed transaction. Independent transactions
run in parallel, while conflicting account accesses retain their canonical
order. Your application does not have to schedule those dependencies itself.

```rust
engine.transaction(transaction).simulate().await; // Inspect without committing.
engine.transaction(transaction).execute().await;  // Wait for the execution result.
engine.transaction(transaction).schedule().await; // Queue without waiting for execution.
```

These are alternative ways to submit work. `execute` reports admission rejection
or the committed result; `schedule` acknowledges queueing only. Cancelling a
wait does not cancel submitted execution. See [transaction semantics](engine/README.md#submitting-transactions).

### 📦 Keep local state durable and mirrored state lightweight

Accounts controlled by the engine live on disk; external-chain copies live in
memory. Storage follows the account lifecycle, so applications do not have to
coordinate the two backends themselves.

Import an account image, optionally applying follow-up instructions in the same
transaction. This lets account activation and the work that depends on it succeed
or fail together.

```rust
engine.account(key).await.materialize(account, actions).await;
```

The host supplies and verifies external-chain state. Replacement respects
delegation and lifecycle rules, not just which image is newer, and requires the
local signer to match the engine authority. If a submitted operation times out,
reacquire and reread the account before retrying. See [account replacement](engine/README.md#account-replacement).

### 📡 React to changes instead of polling

Follow account changes, transaction results, logs, and new blocks as they happen.
Use live events to drive application updates, and retained transaction and block
history for later inspection.

```rust
account_updates = engine.accounts().subscribe(key);
blocks = engine.blocks().subscribe();

update = account_updates.recv().await;
block = blocks.recv().await;
```

Account and block streams have bounded queues; a subscriber that falls behind is
disconnected. Other streams have different delivery guarantees. See
[subscriptions](keeper/README.md#caches-and-subscriptions).

### 🔁 Keep another deployment in step

A source serves execution history over TCP, and followers replay it locally.
This gives you another copy of execution state without building your own
transaction-streaming and snapshot-transfer machinery.

```text
Source:   allow follower identities → serve retained history
Follower: trust source authority → follow its transactions and block boundaries
```

Followers resume from their durable position. When that history has expired,
they receive a snapshot and ask the host to restart from it. This is replication,
not automatic failover or consensus. See [follower setup](replicator/README.md#connecting-a-follower)
for identities, pacing, and connection requirements.

### 🩹 Stop cleanly and recover on restart

Orderly shutdown gives in-flight work time to drain and flushes durable state.
Reopening the same deployment checks account state against retained history,
restores a snapshot when needed, and verifies replay before serving new work.

```rust
shutdown.wait().await;
// Stop accepting new requests.
shutdown.terminate().await;
```

Keep the engine and Tokio runtime alive during draining, and inspect shutdown
results for failures or a required follower restart. Crash recovery needs a valid
retained snapshot when current state is unusable; it does not guarantee complete
queryable history. See [recovery](engine/README.md#startup-and-recovery) and
[durability](ledger/README.md#append-and-read-paths).

## 🧩 Workspace layout

For a closer look at a particular part of Engine:

| Crate | Read more about |
| :-- | :-- |
| [`engine`](engine/README.md) | Embedding Engine, account replacement, and recovery. |
| [`accountsdb`](accountsdb/README.md) | Account storage, snapshots, and scoped zero-copy reads. |
| [`ledger`](ledger/README.md) | Transaction and block history, durability, and retention. |
| [`keeper`](keeper/README.md) | Startup configuration, cached reads, and subscriptions. |
| [`processor`](processor/README.md) | Parallel transaction execution, ordering, and quiescence. |
| [`replicator`](replicator/README.md) | Leader/follower replication and snapshot bootstrap. |
| [`nucleus`](nucleus/README.md) | Shared configuration, runtime types, metrics, and shutdown support. |
| [`solana/*`](solana/README.md) | Runtime forks supporting Engine's account model. |
| `programs/*` | MagicRoot and the v42 test program and interfaces. |
