# `solana-account`

This fork defines the engine's account representation. `Account` is the
fully-owned compatibility form. `AccountSharedData` uses either a heap-owned
`Arc<Vec<u8>>` or a borrowed view into aligned external storage and records
field-level dirty markers.

Equality compares core state and data bytes, ignoring storage form and dirty
markers.

`AccountMode::mutable()` identifies modes writable by user programs.
`AccountMode::authoritative()` separately identifies delegated, ephemeral, and
transient state that the engine owns and higher layers retain in persistent
storage.
`AccountSharedData::set_mode()` is the authoritative lifecycle transition
check: read-only and placeholder accounts may enter any mode except transient,
delegated accounts may enter transient, and transient accounts may resolve to
read-only. Ephemeral accounts may close. Reapplying the current mode is a clean
no-op; invalid mode and slot transitions return `AccountPatchError` with their
source and target context without changing the account.

Slot patches must advance the stored slot. An equal slot is accepted only after
the mode genuinely changed in the same transaction.

Full-account patch sequences set every state flag, establish the exact data
length, and then write data in bounded chunks. Applying such a sequence is an
exact replacement, including when flags are cleared or data shrinks to empty.
Callers set `StateFlags::PROTECTED` when later MagicRoot replacement or
deletion must be rejected. MagicRoot preserves the supplied value and does not
infer it from `AccountMode` or set it during finalization. The flag reuses
persisted bit 1, so the account layout and serialized flag byte do not change.

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
