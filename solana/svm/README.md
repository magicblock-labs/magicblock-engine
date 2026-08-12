# `solana-svm`

This Agave fork is the transaction-level execution entry point. Workspace
`[patch.crates-io]` entries force the dependency graph to use this copy.

The SVM loads required accounts through the caller's
`transaction_processing_callback`, loads required programs, executes through
`solana-program-runtime`, and returns processing results and mutated accounts.
It owns no account storage.

Persistence, commit decisions, deployment policy, and validator batch behavior
remain above this crate. Engine-specific runtime differences are documented in
[`../README.md`](../README.md).

The `frozen-abi` feature is retained as a no-op compatibility stub and forwards
to the corresponding program-runtime feature.
