# SVM

The SVM loads and executes sanitized transactions over caller-provided accounts.
It returns execution results and changed accounts without owning storage or
commit decisions. Engine's processor supplies scheduling and persistence around
this execution boundary.

## Loading and execution

The processor supplies account-loading callbacks, checked resource limits, and
runtime feature configuration. Executable SBF data is already normalized to raw
ELF bytes: loader headers and program-data indirection are resolved outside this
crate. Only programs required by the transaction are loaded.

Callback-based loading allows the same execution machinery to operate on borrowed
accounts for execution or owned copies for simulation. Runtime execution retains
Solana access, rent-state, and lamport-balance checks alongside Engine lifecycle
permissions. Rent-transition relaxation follows supplied features while preserving
Magic-account rules. Requested instruction sysvars that can't encode a transaction
fail loading rather than being replaced with empty data.

The [transaction processor](src/transaction_processor.rs) connects the
[account loader](src/account_loader.rs), [program loader](src/program_loader.rs),
and program runtime. [Final access checks](src/access_permissions.rs) validate
writeback permissions. The [runtime overview](../README.md) covers cross-crate
account and ABI constraints. `frozen-abi` remains a compatibility stub.
