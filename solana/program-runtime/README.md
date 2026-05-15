# `solana-program-runtime`

This Agave fork implements invocation state, CPI translation, SBF VM setup,
sysvar access, logging, serialization, and program-cache primitives. Workspace
`[patch.crates-io]` entries force the dependency graph to use this copy.

Account loading and transaction-level policy belong to `solana-svm`. The
engine-specific direct account mapping, access-violation growth, and CPI
synchronization contracts are documented in [`../README.md`](../README.md).

Changes to `serialization`, CPI account-region replacement, or `vm` error
mapping must remain synchronized with the transaction-context access-violation
handler.

The `frozen-abi` feature is retained as a no-op compatibility stub; this fork
does not derive or consume frozen ABI metadata.
