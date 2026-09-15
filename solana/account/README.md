# `solana-account`

This fork defines the engine's account representation. `Account` is the
fully-owned compatibility form. `AccountSharedData` uses either a heap-owned
`Arc<Vec<u8>>` or a borrowed view into aligned external storage and records
field-level dirty markers.

Equality compares core state and data bytes, ignoring storage form and dirty
markers.

The `testkit` feature exposes borrowed-buffer fixtures and
`testkit::delegated_account(lamports, data, owner)`, which returns a customizable
builder for an explicitly user-mutable test account.

`AccountMode::mutable()` permits user mutation only in delegated and ephemeral
modes. `AccountSharedData::mutable()` is a transaction-final acceptance check:
it also permits transient and closed accounts whose mode changed in the current
transaction. This allows lifecycle writeback, never another program write.
`AccountMode::authoritative()` separately identifies delegated, ephemeral, and
transient state that the engine owns and higher layers retain in persistent
storage.

`AccountSharedData::set_lifecycle()` and `AccountMode::allows_transition()` share
one mode-and-slot rule:

| From | Same or newer slot | Strictly newer slot |
| --- | --- | --- |
| Placeholder | ReadOnly, System, Delegated, Ephemeral, Closed | Placeholder |
| ReadOnly | Delegated, Ephemeral, Closed | ReadOnly, Placeholder |
| System | — | System |
| Delegated | Transient | — |
| Ephemeral | Closed | — |
| Transient | ReadOnly, Placeholder | Delegated |
| Closed | — | — |

Unlisted pairs and slot regressions are rejected. Authoritative accounts cannot
be rematerialized in the same mode, even at a newer slot; ordinary transaction
mutations are unaffected. Errors leave account state and dirty markers unchanged
and identify the invalid mode or slot pair through `AccountPatchError`.

Full-account patch sequences cover non-flag fields, establish the exact data
length, and then write data in bounded chunks. MagicRoot finalization installs
the caller-supplied complete flag value without changing lamports. `StateFlags`
currently contains only `EXECUTABLE`; replacement freshness is enforced by the
caller rather than an account flag.

## Borrowed layout

| Part | Position | Contents |
| --- | --- | --- |
| header | start | sequence and image size |
| pubkey | after header | shared account pubkey |
| image A | after pubkey | core state and data |
| image B | after image A | core state and data |

Borrowed buffers must be 8-byte aligned, match this layout, remain live, and have
unique mutable access for the duration of the borrow. The source may be an mmap,
arena, or test buffer.

The sequence counter selects the active image. Mutation translates active state
into the shadow image; commit advances the sequence to publish it. Writes that
exceed borrowed capacity promote the account to owned storage.
