# `solana-account`

Account types and helpers for the engine.

The crate exposes two representations:

- `Account` is fully owned. Its data lives in a `Vec<u8>` and it stores an
  explicit `rent_epoch`.
- `AccountSharedData` is copy-on-write. It starts borrowed from an aligned
  external buffer and promotes to `OwnedAccount` only when a write outgrows the
  borrowed capacity.

Borrowed storage is layout-bound, not source-bound. It can come from any
aligned buffer, including memory-mapped storage, as long as it:

- is 8-byte aligned
- matches the borrowed layout in `cow::borrowed`
- stays alive and uniquely mutable for the borrow

Borrowed layout:

| Part    | Offset        | Contents                  |
|---------|---------------|---------------------------|
| header  | start         | sequence and image size   |
| pubkey  | after header  | account pubkey            |
| image A | after pubkey  | core state and data bytes |
| image B | after image A | core state and data bytes |

The header picks the active image; the pubkey is shared; the other image is the shadow copy.
`OwnedAccount::units` reports the serialized storage units, and `BorrowedAccount::span`
reports the full borrowed span.

`AccountSharedData` does not store `rent_epoch`. Its readable view reports
`Epoch::MAX`, and constructors that accept a rent epoch ignore it.

This crate owns representation, not routing policy. Higher layers use
`AccountSharedData::mutable()` to decide whether an account belongs in the
persisted or volatile store.
