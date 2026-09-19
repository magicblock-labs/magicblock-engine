# `magic-root-program`

MagicRoot is the engine's privileged native program for patching, finalizing,
and closing accounts. Its wire schema and program id are defined by
`magic-root-interface`.

Every invocation must use the engine `AUTHORITY` as transaction payer/signer.
Direct transaction instructions are accepted. CPI is accepted only when the
immediate caller is registered in the transaction program cache as a builtin
and invokes through `InvokeContext::native_invoke_magic_root`; MagicRoot itself
cannot be the caller. The authorization is private to that exact child invocation,
not inherited by nested CPI or `PostFinalize` actions. Builtins must construct or
validate privileged operations rather than forward arbitrary user payloads.
Ordinary native invocation, authority sponsorship, and native-loader account
metadata do not grant CPI access. Caller authorization and decoding complete
before target-state authorization. The SVM's separate top-level-only privilege
rule is unchanged.

## Instructions

- `Patch` applies one `AccountFieldPatch`. Lamport changes are balanced against
  the authority account, including no-op patches. `Lifecycle` validates mode and
  slot together using the [account lifecycle table](../../solana/account/README.md);
  stale slots and unlisted transitions are rejected. Data patches are limited
  to 10 MiB; larger lengths return `InvalidRealloc`.
- `Finalize` atomically installs the caller-supplied complete flag value and
  loads an executable target into the transaction program cache. It does not
  change lamports; failed executable loading rolls back the installed flags.
- `Delete` transitions ReadOnly, Uninit, or Magic targets to
  `AccountMode::Closed`, immediately hides any transaction-local cached program,
  and removes its shared cache entry after successful execution and access
  validation. Accountsdb removes closed accounts during writeback. Modes that
  cannot transition to closed are rejected.
- `PostFinalize` invokes follow-up instructions through native CPI and is
  placed immediately after the target's `Finalize` by internal composers. It
  rejects any immutable instruction account marked writable and any action that
  targets MagicRoot itself.

After authority and caller checks pass, MagicRoot does not determine whether a
complete account image is stale. Callers must supply current state; slot and
lifecycle validation still apply.

Complete-account patch sequences validate mode and slot together through one
lifecycle patch, following the [account lifecycle table](../../solana/account/README.md).
Rejection aborts the transaction and rolls back every earlier field patch in
that sequence. Post-finalize action failures also roll back replacement and
action account changes.
