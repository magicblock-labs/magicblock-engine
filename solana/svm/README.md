# `solana-svm`

The Engine fork of Agave's transaction execution layer. It loads required
accounts through caller callbacks, executes programs, and returns results and
changed accounts. Callers provide normalized executable program data and the
runtime feature configuration.

This crate does not store accounts or decide which results to commit. Persistence,
deployment policy, consensus, fork choice, and validator batch policy remain
outside the runtime.

Execution retains account access, rent, and balance checks. Rent-transition
relaxation follows the supplied features while preserving the Engine's Magic
account rules. Transactions whose requested instruction sysvar cannot be encoded
fail account loading; the runtime never substitutes an empty sysvar.

See the [runtime-fork contracts](../README.md) for Engine-specific account, ABI,
and execution constraints. The `frozen-abi` feature remains a compatibility stub.
