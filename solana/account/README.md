# Account model

This fork extends Solana accounts with Engine lifecycle states and copy-on-write
access to borrowed storage. Lifecycle tracks whether the ER controls an account;
storage representation lets execution work directly with external buffers. The
type owns mutation rules, while [Accountsdb](../../accountsdb/README.md) owns
persistence and the host owns validation of imported state.

## Account lifecycle

`account.lifecycle()` returns its `AccountState`. Its `is_writable()` and
`is_er_controlled()` predicates determine whether user programs may modify the
account and whether this ER maintains its authoritative image. Both are derived
from lifecycle state, not independently configurable properties. Use
`AccountBuilder::lifecycle()` to select a new account's initial state.

`Delegated` accounts are delegated to this ER; `ErLocal` accounts are authoritative
state created within it. Both allow user mutation. `Undelegating` accounts remain
authoritative while awaiting lifecycle resolution but reject user writes. `Closed`
marks state for storage removal.

Entering `Undelegating` or `Closed` stops user-program writes immediately,
including CPI. The transaction can still commit its earlier changes and the
lifecycle transition.

`Missing` caches the absence of a chain account; `Mirrored` holds a read-only
chain image. `Runtime` covers internal sysvars, features, and precompiles.

Privileged replacement validates state and source slot together. ER-authoritative
images can't be refreshed without a lifecycle transition, even at a newer slot;
otherwise a later external image could overwrite local execution. This restriction
applies to imported images, not writes made by user programs.

| From | Allowed at the same or a newer slot | Requires a strictly newer slot |
| :-- | :-- | :-- |
| Missing | Mirrored, Runtime, Delegated, ErLocal, Closed | Missing |
| Mirrored | Delegated, ErLocal, Closed | Mirrored, Missing |
| Runtime | — | Runtime |
| Delegated | Undelegating | — |
| ErLocal | Closed | — |
| Undelegating | Mirrored, Missing, Closed | Delegated |
| Closed | — | — |

Unlisted transitions and slot regressions fail without changing state or dirty
markers. `ErLocal` state stays local and writable until explicitly closed; it
cannot be frozen or delegated. It must be closed before another image can occupy
its key. The host decides when an account may be created or closed.
See [Engine's import contract](../../engine/README.md#account-replacement) for
stale-image handling and redelegation requirements.

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

[States and copy-on-write behavior](src/cow/mod.rs), the [borrowed layout](src/cow/borrowed.rs),
and [field patches](src/patch.rs) define this model. The `testkit` feature supplies
account and borrowed-storage fixtures. Representation changes also affect transaction
context, VM mapping, and persistence; see the [runtime overview](../README.md).
