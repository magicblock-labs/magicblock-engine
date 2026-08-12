# `magic-root-program`

MagicRoot is the engine's privileged native program for patching, finalizing,
and closing accounts. Its wire schema and program id are defined by
`magic-root-interface`.

Every invocation must use the engine `AUTHORITY` as transaction payer/signer.
Direct transaction instructions are accepted. CPI is accepted only when the
immediate caller is registered in the transaction program cache as a builtin;
MagicRoot itself cannot be the caller. Native-loader account metadata alone does
not grant access. Caller authorization and decoding complete before target-state
authorization. The SVM's separate top-level-only privilege rule is unchanged.

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
- `Finalize` atomically installs the caller-supplied complete flag value and
  loads an executable target into the transaction program cache. It does not
  change lamports; failed executable loading rolls back the installed flags.
- `Delete` transitions read-only, placeholder, or ephemeral targets to
  `AccountMode::Closed`, and accountsdb removes them during writeback. Modes that
  cannot transition to closed are rejected.
- `PostFinalize` invokes follow-up instructions through native CPI and is
  placed immediately after the target's `Finalize` by internal composers. It
  rejects any immutable instruction account marked writable and any action that
  targets MagicRoot itself.

After authority and caller checks pass, MagicRoot does not determine whether a
complete account image is stale. Callers must supply current state; slot and
lifecycle validation still apply.

Complete-account patch sequences apply mode before slot. `AccountSharedData`
marks mode dirty only when its value changes, so a no-op mode patch cannot make
an equal-slot replacement appear fresh. Rejection aborts the transaction, and
therefore rolls back every earlier field patch in that sequence.
