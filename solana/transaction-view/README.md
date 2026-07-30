# `agave-transaction-view`

This Agave fork parses and sanitizes serialized transactions without fully
deserializing them. Workspace `[patch.crates-io]` entries force the dependency
graph to use this copy.

The view owns or borrows the original transaction bytes through
`TransactionData` and caches only framing metadata. Accessors and iterators then
read signatures, account keys, instructions, configuration, and address-table
metadata directly from the validated byte layout.

## Supported formats

- Legacy and v0 use the standard Solana wire layouts.
- V1 uses the Agave V1 layout with instruction headers, contiguous payloads,
  transaction configuration, and trailing signatures.
- `Magicblock` is the Engine-private version `127`. It uses the V1 layout and a
  distinct version prefix; it is not a client-facing Solana transaction version.

For V1 and Magicblock transactions, `message_data()` covers the version byte
through the instruction payloads and excludes the trailing signatures. Engine
construction must sign exactly that range after writing the final version
prefix.

## Limits and parsing invariants

Legacy, v0, and V1 transactions may be at most `u16::MAX` bytes, inclusive.
Magicblock transactions may be at most 16 MiB and may use an instruction trace
length of 255. Other structural limits, including the standard signature and
account-index limits, remain enforced by sanitization.

Compact-u16 values are parsed in their complete canonical one-, two-, or
three-byte form. Absolute offsets and transaction lengths are stored as `u32`.
Fallible parsing validates additions, multiplications, ranges, and conversion to
that representation before any unchecked iterator or typed-slice access. Code
using a sanitized view may rely on those validated frame boundaries.

## Address lookup tables

Address lookup tables are unsupported. A transaction containing any lookup
table entry fails sanitization with `TransactionViewError::AddressLookupMismatch`.
A v0 transaction with an empty lookup list remains valid and requires no loaded
addresses.

## Maintenance constraints

- Preserve standard Legacy and v0 wire compatibility for client-produced
  transactions.
- Keep V1 and Magicblock framing synchronized; only the version, size, account,
  and instruction-trace policies intentionally differ.
- Keep the Magicblock prefix, signed message range, and Engine transaction
  composer synchronized.
- Treat the sanitizer as the authoritative boundary for rejecting address
  lookup tables.
- Validate new frame offsets before exposing them through unchecked accessors.
