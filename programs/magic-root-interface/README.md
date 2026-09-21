# `magic-root-interface`

Instruction definitions and builders for MagicRoot, the engine's privileged
account-management program. Callers can construct account replacement, deletion,
and follow-up operations without depending on the native runtime implementation.

Complete-account replacement composes patches and finalization into one ordered
operation. Finalization installs the supplied flags without changing lamports;
follow-up actions belong immediately after it. Mode and slot must satisfy the
[account lifecycle](../../solana/account/README.md).

The interface does not establish authority or freshness. Callers must verify the
source account state and the provenance of follow-up actions: MagicRoot supplies
the declared action signers and attributes invocation to the declared source.
Only trusted, validated operations may use that privilege.
