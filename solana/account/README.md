# Account model

This fork extends Solana accounts with Engine lifecycle modes and copy-on-write
access to borrowed storage. Lifecycle tracks whether the ER controls an account;
storage representation lets execution work directly with external buffers. The
type owns mutation rules, while [Accountsdb](../../accountsdb/README.md) owns
persistence and the host owns validation of imported state.

## Account lifecycle

Lifecycle separates authority from mutability. `Delegated` accounts are delegated
to this ER; `Magic` accounts are authoritative state created within it. Both allow
user mutation. `Transient` accounts remain authoritative while awaiting lifecycle
resolution but reject user writes. `Closed` marks state for storage removal.

Transitions to transient or closed revoke mutation immediately, including in CPI.
The transaction-final writeback guard can accept a dirty transition into those
modes without permitting further program writes. Other modes represent cached
external absence (`Uninit`), a read-only mirror (`ReadOnly`), and internal sysvar,
feature, or precompile state (`System`).

Privileged replacement validates mode and source slot together. Authoritative
state can't be refreshed in the same mode, even at a newer slot; otherwise a later
external image could overwrite local execution. Ordinary transaction mutations
aren't replacements and don't follow that restriction.

| From | Allowed at the same or a newer slot | Requires a strictly newer slot |
| :-- | :-- | :-- |
| Uninit | ReadOnly, System, Delegated, Magic, Closed | Uninit |
| ReadOnly | Delegated, Magic, Closed | ReadOnly, Uninit |
| System | — | System |
| Delegated | Transient | — |
| Magic | Closed | — |
| Transient | ReadOnly, Uninit, Closed | Delegated |
| Closed | — | — |

Unlisted transitions and slot regressions fail without changing state or dirty
markers. Magic state must be explicitly closed before another image can occupy
its key, regardless of token balance. The host validates creation and closure
eligibility, including application-specific token rules. [Engine's import path](../../engine/README.md#account-replacement)
owns observation-based deduplication and the evidence required for redelegation.

## Borrowed storage

Borrowed accounts use an 8-byte-aligned buffer containing active and shadow images.
Mutation copies into the shadow; commit publishes it, reset abandons it, and rollback
undoes an already published commit. Rollback is valid only after commit. Growth
beyond borrowed capacity promotes to owned storage; shared owned data uses
`Arc::make_mut`.

Backing buffers must satisfy layout requirements, remain live at a stable address,
and provide exclusive writer access during mutation. Reads racing publication
require the sequence-lock protocol. Each unsafe API specifies the exact lifetime
and exclusion requirements; the borrowed view doesn't independently own its storage.

Dirty markers identify changed fields. Complete replacement patches non-flag fields,
then privileged finalization installs all supplied flags without changing lamports.
The host remains responsible for source freshness. Equality compares account state
and bytes, not storage representation or dirty markers. Lifecycle semantics and
binary encoding must agree across producers and consumers.

[Modes and copy-on-write behavior](src/cow/mod.rs), the [borrowed layout](src/cow/borrowed.rs),
and [field patches](src/patch.rs) define this model. The `testkit` feature supplies
account and borrowed-storage fixtures. Representation changes also affect transaction
context, VM mapping, and persistence; see the [runtime overview](../README.md).
