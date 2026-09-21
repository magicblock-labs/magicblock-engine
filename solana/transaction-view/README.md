# `agave-transaction-view`

Parse and sanitize serialized transactions without fully deserializing them.
This Engine fork of Agave provides views over owned or borrowed transaction bytes
for execution, signature verification, and instruction inspection.

## Supported formats

Legacy, v0, and V1 retain their standard wire layouts. Magicblock is an
Engine-private, V1-shaped format for larger atomic account operations; it is not
a client-facing Solana transaction version. Its larger size allowance does not
relax standard transaction or instruction-sysvar limits.

Signing must cover the format's exact message range after the final version
prefix is written, excluding signatures themselves. Private-format construction,
parsing, and execution policy must stay synchronized.

## Validation boundary

Parsing establishes checked frame boundaries before exposing unchecked views.
Sanitization enforces account-key uniqueness and structural limits; instructions
may still reference the same account index more than once. Callers must not treat
successfully parsed but unsanitized data as ready for execution.

Address lookup tables are unsupported and nonempty lookup entries fail
sanitization. A v0 transaction with an empty lookup list remains valid.

Preserve canonical compact-u16 parsing and standard wire compatibility when
changing the framing code. See the [runtime-fork contracts](../README.md) for
private-format and runtime compatibility constraints.
