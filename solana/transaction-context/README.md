# Transaction context

Transaction context owns account access and instruction state for one execution,
including CPI. It enforces runtime borrowing and tracks return data, account
changes, and execution limits. This is where Engine's account model meets
instruction-level Solana permissions.

## Borrowing and remapping

Direct VM mapping allows copy-on-write and growth to change an account's backing
pointer during execution. Accounts live in `UnsafeCell`s guarded by explicit borrow
counters, allowing the access handler to remap data while runtime borrow rules
remain enforced. `AccountRef` and `AccountRefMut` release counters on drop.

The context records touched accounts, total data resize, and instruction lamport
deltas. Its execution record returns keyed accounts, return data, touched count,
and resize delta to the caller rather than persisting them. Every account reference
must be released before deconstruction; failure to unwrap the shared account state
indicates a lifetime bug.

## Lifecycle permissions

Instruction writes require writable [account access](../account/README.md#account-lifecycle)
in addition to Solana ownership and instruction-writability checks. Entering
`Undelegating` or `Closed` blocks further writes, including CPI, while allowing
the transaction to commit the lifecycle transition.

Setting an unchanged balance, owner, or executable flag skips the account-access
check after the usual Solana checks pass. For privileged account replacement, see
[MagicRoot authorization](../../programs/magic-root-program/README.md#authorization).

[Transaction state](src/transaction.rs) owns the VM access handler;
[transaction accounts](src/transaction_accounts.rs) own borrowing, and
[instruction access](src/instruction.rs) applies mutation checks. Remapping changes
must remain compatible with [serialization and CPI](../README.md#direct-vm-mapping).
