# Transaction views

Transaction views expose serialized transaction fields without full deserialization.
Engine uses them for account-dependency scheduling, signature verification, and
instruction access. This fork adds Engine-private framing while preserving standard
Solana wire layouts.

## Validation boundary

Parsing establishes checked frame boundaries before exposing unchecked views.
Sanitization validates account-key uniqueness, indices, and structural limits;
parsed but unsanitized data isn't ready for execution. Static account keys must
be unique, although an instruction can reference the same index more than once.

Nonempty address lookup entries fail sanitization because Engine doesn't resolve
lookup tables. Empty v0 lookup lists remain valid. Resolution support would need
ingress, sanitization, scheduling, and simulation changes together, not just parser support.

## Private framing

Legacy, v0, and V1 retain standard layouts. Magicblock is a private V1-shaped format
with version 127 and a 16 MiB limit, used for atomic account imports. It isn't a
client-facing Solana version. Its larger framing doesn't enlarge standard
instruction-sysvar encoding or remove runtime trace limits; those constraints are
collected in the [runtime overview](../README.md#private-transaction-framing).

Signatures cover the exact message range, excluding signatures themselves. The
final version prefix must be written before signing. Private construction,
parsing, sanitization, and execution policy must stay synchronized.

[Views](src/transaction_view.rs) and [sanitization](src/sanitize.rs) define the
structural boundary. Framing changes must preserve canonical compact-u16 parsing,
checked `u32` offsets, and standard wire compatibility.
