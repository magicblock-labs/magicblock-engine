# `magic-root-program`

MagicRoot is the engine's privileged native program for replacing, finalizing,
and closing accounts. It also supports atomic follow-up actions after replacement.
Use [the interface crate](../magic-root-interface/README.md) to construct its
instructions.

## Authorization

Every invocation requires the engine authority as transaction payer and signer.
Direct authority instructions are accepted. CPI additionally requires a builtin
caller using the runtime's explicit MagicRoot authorization; sponsorship or
native-loader ownership alone is insufficient.

Authorization applies only to the exact child invocation. Nested calls, later
siblings, and follow-up actions do not inherit it, and MagicRoot cannot authorize
itself recursively. Builtins must construct or validate privileged operations,
not forward arbitrary user payloads. The separate SVM top-level-only privilege
rule still applies.

## Account operations

Patches obey the [account lifecycle](../../solana/account/README.md), reject stale
slots and unsupported transitions, and balance lamport changes against the
authority account. Finalization installs the complete supplied flags and makes
executable state available to execution without changing lamports. Closing an
account revokes its availability, including cached executable state.

Replacement and follow-up account changes are transactional: rejected patches,
failed executable loading, or failed actions roll them back. Follow-up actions
cannot target MagicRoot or mark immutable accounts writable.

Authority is not proof that an account image is current. The host remains
responsible for freshness, replacement eligibility, and action provenance.
