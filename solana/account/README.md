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

`AccountMode::mutable()` permits user mutation only in delegated and Magic
modes. `AccountSharedData::mutable()` is a transaction-final acceptance check:
it also permits transient and closed accounts whose mode changed in the current
transaction. This allows lifecycle writeback, never another program write.
`AccountMode::authoritative()` separately identifies delegated, Magic, and
transient state that the engine owns and higher layers retain in persistent
storage.

`AccountSharedData::set_lifecycle()` and `AccountMode::allows_transition()` share
one mode-and-slot rule:

| From | Same or newer slot | Strictly newer slot |
| --- | --- | --- |
| Uninit | ReadOnly, System, Delegated, Magic, Closed | Uninit |
| ReadOnly | Delegated, Magic, Closed | ReadOnly, Uninit |
| System | — | System |
| Delegated | Transient | — |
| Magic | Delegated, Closed | — |
| Transient | ReadOnly, Uninit | Delegated |
| Closed | — | — |

Unlisted pairs and slot regressions are rejected. Authoritative accounts cannot
be rematerialized in the same mode, even at a newer slot; ordinary transaction
mutations are unaffected. Errors leave account state and dirty markers unchanged
and identify the invalid mode or slot pair through `AccountPatchError`.

Magic represents state that exists only inside the ER, including locally created
ATAs. A privileged operation may close it or replace it with delegated state.
The host owns creation and replacement eligibility; for ATAs, this includes
preventing replacement while funded. Engine does not parse token data or require
a positive token balance at transaction end.

Uninit and Magic retain the numeric discriminants and binary variant indices of
the former Placeholder and Ephemeral modes (0 and 4). Rust variant names and
name-based serialization change. Replication peers must agree on the new lifecycle
semantics before using Magic-to-Delegated replacement.

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
