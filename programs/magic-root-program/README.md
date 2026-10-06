# MagicRoot

MagicRoot is Engine's native builtin for privileged account replacement, finalization,
and closure. These operations run inside transactions so they share execution
ordering and rollback with ordinary work, rather than bypassing the runtime with
administrative storage writes.

[Engine](../../engine/README.md#account-replacement) uses this path to install account
images and execute follow-up actions atomically. The [interface crate](../magic-root-interface/README.md)
owns instruction definitions; this crate owns authorization and runtime behavior.

## Account operations

Replacement applies bounded field patches followed by finalization. Patches
validate [lifecycle mode and slot](../../solana/account/README.md#account-lifecycle)
and balance lamport changes against the authority's sponsor account. Finalization
installs the complete flags without changing lamports and loads executable state
into the program cache. Closure marks the account for removal and immediately
hides cached executable state.

Rejected patches, failed executable loading, and failed follow-up actions roll
back their account changes. Follow-up actions can't target MagicRoot or mark
immutable accounts writable. The host validates source freshness, replacement
eligibility, and action provenance; the builtin enforces the supplied operation,
not the external evidence behind it.

## Authorization

Every invocation requires the engine authority as transaction payer and signer.
Top-level authority instructions are accepted directly. CPI additionally requires
a builtin caller using the runtime's explicit MagicRoot authorization. Sponsorship,
native-loader ownership, and ordinary native CPI are insufficient.

Authorization applies only to the exact child invocation. Descendants, later
siblings, and follow-up actions don't inherit it, and MagicRoot can't authorize
itself recursively. Builtins must construct or validate operations, not forward
arbitrary user payloads. The SVM's separate final-writeback exemption requires all
top-level instructions to target MagicRoot; authorized CPI doesn't grant that
transaction-wide privilege.

[Authorization and dispatch](src/processor.rs) lead into [account operations](src/account.rs)
and [follow-up invocation](src/post_finalize.rs).
