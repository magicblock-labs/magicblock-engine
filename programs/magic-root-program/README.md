# `magic-root-program`

MagicRoot is the engine's privileged native program for patching, finalizing,
and closing accounts. Its wire schema and program id are defined by
`magic-root-interface`.

Every invocation must use the engine `AUTHORITY` as transaction payer/signer.
Direct transaction instructions are accepted. CPI is accepted only when the
immediate caller is registered in the transaction program cache as a builtin;
native-loader account metadata alone does not grant access. Caller authorization
and decoding complete before target-state authorization. The SVM's separate
top-level-only privilege rule is unchanged.

## Instructions

- `Patch` applies one `AccountFieldPatch`. Lamport changes are balanced against
  the authority account, including no-op balance patches. Slot patches must
  advance the stored slot; an equal slot is accepted only when an earlier patch
  in the same transaction genuinely changed the account mode. Older slots are
  always rejected. Data patches may produce at most 10 MiB of account data;
  larger lengths return `InvalidRealloc`. Account mode transitions use the
  policy owned by `AccountSharedData`: read-only and placeholder accounts may
  transition to any mode except transient, transient accounts may resolve only
  to read-only, and delegated accounts may transition only to transient.
  Ephemeral accounts may close. Other mode changes are rejected.
- `Finalize` loads an executable target into the transaction program cache. It
  does not change account state or lamports.
- `Delete` rejects a `PROTECTED` target. Otherwise, it transitions
  read-only, placeholder, or ephemeral targets to `AccountMode::Closed`, and
  accountsdb removes them during writeback. Modes that cannot transition to
  closed are rejected.
- `PostFinalize` invokes follow-up instructions through native CPI. A protected
  target accepts it only when the immediately preceding trace
  entry is MagicRoot `Finalize` for the same target, stack height, and caller.
  It rejects any immutable instruction account marked writable.

Callers supply the `PROTECTED` flag as part of the account state when they
want to prevent later replacement or deletion. MagicRoot preserves that value;
it does not infer authority from `AccountMode` or set the flag during
`Finalize`. Mutations of a protected target return `PrivilegeEscalation`,
including calls made through a builtin, except for the narrow `PostFinalize`
path that keeps create-with-actions atomic without permitting a later
standalone mutation.

Complete-account patch sequences apply mode before slot. `AccountSharedData`
marks mode dirty only when its value changes, so a no-op mode patch cannot make
an equal-slot replacement appear fresh. Rejection aborts the transaction, and
therefore rolls back every earlier field patch in that sequence.
