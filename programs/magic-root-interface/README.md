# MagicRoot interface

This crate defines the instruction format shared by Engine and the
[MagicRoot builtin](../magic-root-program/README.md). It separates composition of
privileged account operations from their runtime implementation.

## Replacement sequence

Complete-account replacement composes bounded patches followed by finalization.
Finalization installs the full flags without changing lamports; follow-up actions
belong immediately after it. Engine submits the sequence as one private transaction,
while the builtin validates and executes it against the [account lifecycle](../../solana/account/README.md#account-lifecycle).

## Action provenance

`PostFinalize` carries privileges as well as instructions. MagicRoot invokes each
action with the declared source program as effective caller and supplies its declared
signers. The host must verify authority, source freshness, and action provenance
before submission; this payload must not be copied from untrusted input.

[Definitions and composition](src/lib.rs) own the wire format. The `PostFinalize`
Rustdoc specifies the provenance contract, including cases where the source program
and the finalized account's owner differ.
