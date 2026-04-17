# MagicBlock Engine

Execution engine for ephemeral rollups.

The workspace is centered on one account model:

- `mutable` accounts are under ER-exclusive control, like blockchain-locked state.
- `immutable` accounts are chain-owned or service-owned, like sysvars.

The main crates are:

- `solana/account`: shared account primitives and copy-on-write storage
- `accountsdb`: account routing and storage for mutable and immutable state
- `solana/transaction-context`: instruction and transaction state
- `solana/program-runtime`: runtime and VM support
- `solana/svm`: load and execute orchestration

## Account Routing

`accountsdb` routes accounts by mutability:

- mutable accounts are persisted as the authoritative copy
- immutable accounts are kept in volatile memory
- borrowed immutable accounts also hit persisted storage so stale persisted entries are removed
- owned mutable accounts also hit volatile storage so stale volatile entries are removed

## Storage Contract

Persisted state uses a mapped file plus LMDB index. Volatile state stays in
memory. `solana-account::AccountSharedData` is the shared representation; the
policy for routing it lives above that crate.

## Workspace Rule

Keep the ER model above the implementation. Do not reintroduce validator flow,
consensus, or fee logic unless a forked dependency forces it.
