# `solana-transaction-context`

Account and instruction state for a single executing transaction. This Engine
fork of Agave enforces runtime borrowing while tracking account changes, return
data, and execution limits across direct calls and CPI.

Instruction writes require a currently mutable account mode in addition to
Solana ownership and writability checks. Transitions to transient or closed take
effect immediately, including across CPI. Dirty lifecycle markers permit
writeback of the transition, not further program writes. Setting an unchanged
lamport, owner, or executable value does not require mode permission after the
usual Solana checks pass; privileged raw operations have a separate boundary.

Account data may grow or be remapped during execution, so runtime references must
respect borrow lifetimes. Release all account references before deconstructing
the context. See the [runtime-fork contracts](../README.md) for the direct-mapping
and cross-crate safety requirements.
