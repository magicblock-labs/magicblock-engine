# Account model

This fork extends Solana accounts with Engine lifecycle modes and copy-on-write
access to borrowed storage. Lifecycle tracks whether the ER controls an account;
storage representation lets execution work directly with external buffers. The
type owns mutation rules, while [Accountsdb](../../accountsdb/README.md) owns
persistence and the host owns validation of imported state.

## Account lifecycle

`AccountMode` combines residual classification (`AccountKind`), ordinary local
access (`AccessMode`), and ongoing synchronization policy (`DataFlow`) in two
bytes. Named constants expose the supported combinations:

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

The default is `Uninit`. An account delegated to another validator uses `Base`;
its delegation status is not separately retained by the mode. Callers can compare
complete modes with `account.is(AccountMode::Magic)` or use typed getters and
predicates. Mode fields are private, and decoding admits only supported states.

`is_writable()` controls ordinary program mutation. `can_writeback()` on shared
account data also accepts this transaction's dirty transitions into Undelegating
or Closed, without authorizing further writes, including across CPI.

`is_authoritative()` protects Regular accounts whose policy is None or MagicToBase:
Magic, active local delegation, and undelegating state. Base mirrors remain
refreshable and evictable. Both System modes remain volatile and survive mirror
reset. The host establishes delegation authority, commit targets, and trusted
lifecycle evidence; this type stores no keys or proof of that authority.

DataFlow describes ongoing account-state synchronization between the base chain
and the Magic Validator. Magic names the validator here, independently of the
account mode. Initial cloning and
delegation-status observation are separate. MagicToBase remains the policy during
handoff without authorizing ordinary writes or arbitrary new commits.

Mode and slot are validated together. **System** below means either System
constant; the slot requirement also applies when changing between their flows.

| From | Same or newer slot | Strictly newer slot |
| --- | --- | --- |
| Uninit | Base, System, Delegated, Magic, Closed | Uninit |
| Base | Delegated, Magic, Closed | Base, Uninit |
| System | — | System |
| Delegated | Undelegating | — |
| Magic | Closed | — |
| Undelegating | Base, Uninit, Closed | Delegated |
| Closed | — | — |

Unlisted transitions and slot regressions are rejected without changing state or
dirty markers. Authoritative accounts cannot be replaced in the same mode, even
at a newer slot; ordinary transaction mutations are unaffected. Access and flow
changes are part of the complete lifecycle transition, not independent setters.

Magic represents locally authoritative ER state without direct base commitment
through protocol delegation. A base counterpart may exist independently. Magic
must be closed before another image can occupy its key; the host validates
creation and closure eligibility. Empty token balances do not invalidate Magic
accounts, and this crate does not interpret token data.

[Engine's import path](../../engine/README.md#account-replacement) owns
observation-based deduplication and the evidence required for redelegation.

Closed is a deletion tombstone, retained in the transaction until storage removes
the image. It means deletion has occurred, rather than eligibility for closure.

The binary mode encoding is `[kind_byte, policy_byte]`: policy bit 0 is Access,
bits 1–2 are DataFlow, and the remaining bits are reserved. Decoders reject
invalid tags, reserved bits, and unsupported combinations before constructing a
mode. The mode and mmap core remain two and 56 bytes respectively. This encoding
replaces the unpublished kind/access prototype; existing prototype state must
be discarded. Accountsdb and ledger format version 2 remain the new format of
this unreleased refactor; replication protocol version 1 is unchanged.

Flags are supplied as a complete value during privileged finalization, which
does not change lamports. They are not evidence of source freshness. Producers
and consumers must agree on lifecycle semantics as well as binary encoding.

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
