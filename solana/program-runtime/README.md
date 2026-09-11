# `solana-program-runtime`

Agave 4.2.2 compatibility removes the obsolete modular-exponentiation execution
cost fields and the language-specific signer method from `SyscallInvokeSigned`.
C and Rust signer translation share one `VmSlice` implementation; the previous
free-function names remain aliases. Mutable CPI slice translation is unsafe:
callers must uphold reference uniqueness and backing-storage lifetimes.

This Agave fork implements invocation state, CPI translation, SBF VM setup,
sysvar access, logging, serialization, and program-cache primitives. Workspace
dependencies that select `solana-program-runtime` use this workspace copy.

Account loading and transaction-level policy belong to `solana-svm`. The
engine-specific direct account mapping, access-violation growth, and CPI
synchronization contracts are documented in [`../README.md`](../README.md).

Changes to `serialization`, CPI account-region replacement, or `vm` error
mapping must remain synchronized with the transaction-context access-violation
handler.

The `frozen-abi` feature is retained as a no-op compatibility stub; this fork
does not derive or consume frozen ABI metadata.
