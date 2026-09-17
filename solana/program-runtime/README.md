# `solana-program-runtime`

This Agave fork implements invocation state, CPI translation, SBF VM setup,
sysvar access, logging, serialization, and program-cache primitives. Workspace
dependencies that select `solana-program-runtime` use this workspace copy.

Account loading and transaction-level policy belong to `solana-svm`. The
engine-specific direct account mapping, access-violation growth, and CPI
synchronization contracts are documented in [`../README.md`](../README.md).

Keep `serialization`, CPI account-region replacement, `vm` error mapping, and
the transaction-context access-violation handler synchronized. C and Rust signer
translation share `VmSlice`; mutable slice translation is unsafe and requires
unique references and live backing storage.

`InvokeContext::native_invoke_magic_root` explicitly authorizes only its exact
child instruction to enter MagicRoot. Builtins must construct or validate the
privileged operation; this method is not for forwarding untrusted instructions.
It reuses normal CPI preparation and execution without granting additional
signers. A private instruction-trace index scopes authorization and is restored
on success or error. Nested CPI, later siblings, and `PostFinalize` actions do
not inherit it. Ordinary native calls, including provenance-attributed calls,
do not grant MagicRoot authorization.

The `frozen-abi` feature is retained as a no-op compatibility stub; this fork
does not derive or consume frozen ABI metadata.
