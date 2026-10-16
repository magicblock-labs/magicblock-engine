# Account model

This fork extends Solana accounts with Engine lifecycle modes and copy-on-write
access to borrowed storage. Lifecycle tracks whether the ER controls an account;
storage representation lets execution work directly with external buffers. The
type owns mutation rules, while [Accountsdb](../../accountsdb/README.md) owns
persistence and the host owns validation of imported state.

## Account lifecycle

AccountMode stores AccountKind, AccessMode, and DataFlow in two bytes. The account
core remains 56 bytes. Named constants select the supported combinations:

| Constant | Kind | Access | DataFlow |
| --- | --- | --- | --- |
| `Uninit` | Uninit | ReadOnly | BaseToMagic |
| `Base` | Regular | ReadOnly | BaseToMagic |
| `Delegated` | Regular | Writable | MagicToBase |
| `Undelegating` | Regular | ReadOnly | MagicToBase |
| `Magic` | Regular | Writable | None |
| `System` | System | ReadOnly | None |
| `MirroredSystem` | System | ReadOnly | BaseToMagic |
| `Closed` | Closed | ReadOnly | None |

Uninit is the default. Accounts delegated elsewhere use Base. Magic can coexist
with an independent base account; its local state has no direct delegated commit
relationship with base. System accounts are volatile and survive mirror reset.

Access controls ordinary program writes. `can_writeback()` also accepts dirty
transitions into Undelegating or Closed, without permitting subsequent program
writes, including across CPI. Magic, Delegated, and Undelegating remain locally
authoritative; Closed is a deletion tombstone removed by storage.

DataFlow describes ongoing state sync with base; Magic names the validator here.
Initial cloning and delegation-status observation are separate. The host supplies
authority, commit targets, source freshness, and evidence permitting transitions.

Lifecycle changes validate the complete mode and source slot atomically. System
below means either System constant, including changes between their flows.

| From | Same or newer slot | Strictly newer slot |
| --- | --- | --- |
| Uninit | Base, System, Delegated, Magic, Closed | Uninit |
| Base | Delegated, Magic, Closed | Base, Uninit |
| System | — | System |
| Delegated | Undelegating | — |
| Magic | Closed | — |
| Undelegating | Base, Uninit, Closed | Delegated |
| Closed | — | — |

Unlisted pairs and slot regressions fail without changing state or dirty markers.
Authoritative images cannot be replaced in the same mode, even at a newer slot;
ordinary execution is unaffected. Magic must be closed before replacement,
regardless of token balance. See [Engine's import contract](../../engine/README.md#account-replacement)
for deduplication and redelegation evidence.

The encoding is `[kind, policy]`: policy bit 0 is access and bits 1–2 are flow.
Decoders reject invalid tags, reserved bits, and unsupported combinations.
Accountsdb and ledger format 2 require fresh state; there is no automatic migration.
Replication protocol version remains 1, so peers must use the matching encoding.

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

[Modes](src/mode.rs), [copy-on-write behavior](src/cow/mod.rs), the [borrowed layout](src/cow/borrowed.rs),
and [field patches](src/patch.rs) define this model. The `testkit` feature supplies
account and borrowed-storage fixtures. Representation changes also affect transaction
context, VM mapping, and persistence; see the [runtime overview](../README.md).
