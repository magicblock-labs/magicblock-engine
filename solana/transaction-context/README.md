# `solana-transaction-context`

This Agave fork defines the account and instruction state used by one executing
transaction. Workspace `[patch.crates-io]` entries force the dependency graph to
use this copy.

`TransactionAccounts` stores account cells behind runtime borrow counters.
`AccountRef` and `AccountRefMut` enforce those counters while VM access handlers
can resize and remap an account's directly mapped data. The context also tracks
touched accounts, resize and lamport deltas, return data, instruction state, and
execution limits.

Ordinary instruction mutations require a currently mutable account mode, in
addition to Solana ownership and instruction-writability checks. A transition to
transient or closed takes effect immediately, including across CPI. Unchanged
lamport, owner, and executable values do not require mode permission after their
existing Solana checks pass. Dirty lifecycle markers remain intact for writeback;
they never authorize further program writes. Privileged raw account operations
remain outside this instruction-level boundary.

The direct-mapping and access-violation contracts are documented in
[`../README.md`](../README.md). All account references must be released before a
`TransactionContext` is deconstructed.
