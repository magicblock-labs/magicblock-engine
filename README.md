<h1 align="center">MagicBlock Engine</h1>

<p align="center">
  <b>Solana execution and state management for ephemeral rollups.</b>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-Apache--2.0-6d4bd6" alt="License Apache-2.0">
  <img src="https://img.shields.io/badge/rust-1.96.1-orange?logo=rust" alt="Rust 1.96.1">
  <img src="https://img.shields.io/badge/edition-2024-blue" alt="Edition 2024">
  <img src="https://img.shields.io/badge/Solana-SVM-14F195?logo=solana&logoColor=white" alt="Solana SVM">
  <img src="https://img.shields.io/badge/status-experimental-yellow" alt="Status experimental">
  <img src="https://img.shields.io/badge/version-0.14.2-lightgrey" alt="Version 0.14.2">
</p>

<p align="center">
  <a href="#-engine-in-practice">✨ API tour</a> ·
  <a href="#-workspace-layout">🧩 Workspace</a> ·
  <a href="solana/README.md">🔬 Runtime internals</a>
</p>

---

Engine is the execution core behind [MagicBlock Validator (MBV)](https://github.com/magicblock-labs/magicblock-validator).
It runs Solana transactions over account state in an ephemeral rollup (ER), bringing
together the SVM, parallel scheduling, persistent storage, live events, and replication.

MBV takes care of base-chain integration and commit policy; Engine handles local
execution and state. Its blocks mark execution boundaries, not consensus or
confirmation. This repo is where those execution and storage pieces come together.

## ✨ Engine in practice

A few API examples give a feel for what Engine handles. Configuration and the finer
contracts live in the linked crate READMEs.

### 🚀 Start execution with recovery built in

With storage, authority, and initial state configured in a `KeeperBuilder`, one
handle starts the stores, recovery, execution workers, and block pacing:

```rust
use engine::Engine;
use nucleus::shutdown::ShutdownManager;

let mut shutdown = ShutdownManager::default();
let engine = Engine::new(builder, None, &mut shutdown).await?;
```

Engine can generate its own block boundaries or follow an external pacer.
[Startup and orchestration →](engine/README.md#startup-and-recovery)

### ⚡ Execute in parallel, or simulate first

Independent transactions run concurrently; conflicting account accesses keep their
input order. Preview a transaction, then execute it against local state:

```rust
let simulation = engine.transaction(transaction.clone())?.simulate(true).await?;
let result = engine.transaction(transaction)?.execute().await?;
```

| Mode | Completion boundary |
| :-- | :-- |
| `simulate(sigverify)` | Execution on account copies, without committing; verifies transaction signatures when requested. |
| `execute` | Admission rejection or the committed execution result. |
| `schedule` | Queueing acknowledgment, without waiting for admission or execution. |

For queue-only submission, use `engine.transaction(transaction)?.schedule().await?`.
Execution and simulation results can contain transaction-level failures even when
the Engine call succeeds. Cancelling a wait doesn't cancel submitted execution.
[Transaction semantics →](engine/README.md#submitting-transactions)

### 📦 Import accounts atomically

Delegated and ER-created state is persisted; rebuildable external-chain mirrors
stay in memory. Account imports can include follow-up instructions in the same
transaction, so activation and the work that depends on it succeed or fail together:

```rust
engine.account(key).await?.materialize(account, actions).await?;
```

MBV validates the external state; Engine enforces lifecycle and slot rules.
Materialization can also skip an already handled or superseded image.
[Account lifecycle and imports →](engine/README.md#account-replacement)

### 📡 Follow live changes

Subscribe to account changes, transaction results, logs, and blocks. Retained
history supports queries and replay alongside those live streams:

```rust
let mut account_updates = engine.accounts().subscribe(key);
let mut blocks = engine.blocks().subscribe();

let update = account_updates.recv().await;
let block = blocks.recv().await;
```

Account and block subscribers have bounded queues; slow receivers are disconnected.
Other streams have their own delivery contracts.
[Subscriptions →](keeper/README.md#caches-and-subscriptions) · [History →](ledger/README.md)

### 🔁 Replay execution on a follower

A source serves ordered history over TCP. Followers authenticate it, re-execute
transactions locally, and check history and persisted-state boundaries:

```text
Source:   allow follower identities → serve retained history
Follower: trust source authority → replay transactions and block boundaries
```

Followers resume from a synchronized position while history is retained. Otherwise,
snapshot transfer stages state for a host-managed restart. This is execution
replication, not consensus between peers. [Replication →](replicator/README.md)

### 🩹 Drain cleanly and recover on restart

Coordinated shutdown drains work and synchronizes storage. Keep Engine and the
async runtime alive while services finish:

```rust
let reason = shutdown.wait().await;
// Stop accepting new requests.
let reason = reason.combine(shutdown.terminate().await);
```

On restart, Engine validates stored state and, when needed, restores a retained
snapshot and replays history before accepting new work. Recovery depends on retained
state; it doesn't guarantee complete queryable crash-tail history, and execution
completion alone isn't a disk-sync guarantee.
[Recovery →](engine/README.md#startup-and-recovery) · [Durability →](ledger/README.md#append-and-read-paths)

## 🔬 The runtime underneath

Engine uses local forks of Agave's runtime to support its account lifecycle and
borrowed storage. Account data is mapped directly into the SBF VM, while storage
and commit decisions stay outside the SVM. These adaptations let execution work
with Engine's state model without importing validator machinery.

The [runtime overview](solana/README.md) covers the fork boundaries, direct mapping,
and compatibility constraints.

## 🧩 Workspace layout

Start with the [Engine README](engine/README.md) for the architecture, or pick a
crate below. Each README explains its role, design constraints, and source entry
points. Validator setup and surrounding services live in [MBV](https://github.com/magicblock-labs/magicblock-validator).

| Crate | Responsibility |
| :-- | :-- |
| [`engine`](engine/README.md) | Service orchestration, transaction ingress, account imports, and recovery. |
| [`processor`](processor/README.md) | Dependency scheduling, SVM execution, simulation, and execution barriers. |
| [`keeper`](keeper/README.md) | Coordination of storage, caches, subscriptions, and runtime configuration. |
| [`accountsdb`](accountsdb/README.md) | Current account state, persisted and volatile storage, snapshots, and checksums. |
| [`ledger`](ledger/README.md) | Execution history, indexing, replay, durability, and retention. |
| [`replicator`](replicator/README.md) | Authenticated source/follower streams and snapshot bootstrap. |
| [`nucleus`](nucleus/README.md) | Shared configuration, runtime and record types, metrics, and service lifecycle. |
| [`solana/*`](solana/README.md) | Runtime forks for Engine's account model and VM mapping. |
| `programs/*` | The privileged [MagicRoot builtin](programs/magic-root-program/README.md) and [v42 SBF test fixture](programs/v42-calculator-program/README.md). |
