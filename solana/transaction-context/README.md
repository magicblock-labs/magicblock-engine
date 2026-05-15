# `solana-transaction-context`

This Agave fork defines the account and instruction state used by one executing
transaction. Workspace `[patch.crates-io]` entries force the dependency graph to
use this copy.

`TransactionAccounts` stores account cells behind runtime borrow counters.
`AccountRef` and `AccountRefMut` enforce those counters while VM access handlers
can resize and remap an account's directly mapped data. The context also tracks
touched accounts, resize and lamport deltas, return data, instruction state, and
execution limits.

The direct-mapping and access-violation contracts are documented in
[`../README.md`](../README.md). All account references must be released before a
`TransactionContext` is deconstructed.

The instruction trace retains each instruction's program, accounts, data,
stack height, and caller. MagicRoot uses that context to restrict
`PostFinalize` on a protected target to the sibling instruction
immediately following its matching `Finalize`.
