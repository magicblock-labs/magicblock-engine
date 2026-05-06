# `v42-calculator-program`

This SBF test program evaluates the postfix wire format from
`v42-calculator-interface`. It exercises account-data reads, the `Clock` sysvar,
checked arithmetic, recursive CPI, return data, writable account output, and
direct lamport transfers.

The crate uses Rust 2021 and upstream minimal program crates so it remains
compatible with `cargo build-sbf`. Keeper's build script produces
`target/deploy/v42_calculator_program.so` for its testkit.

## Execution model

The crate entrypoint dispatches to the calculator and transfer domains. The
calculator owns expression evaluation and value encoding, transfer owns signed
balance and value movement, and error owns the stable calculator error codes.

The evaluator processes literals, account `i64` values, clock timestamps,
arithmetic opcodes, and length-prefixed nested calls on a fixed-capacity stack.
Malformed input, stack errors, arithmetic errors, missing accounts, and short
buffers return stable `ProgramError::Custom` codes from `CalcError`.

A top-level invocation writes the final little-endian `i64` to account zero. A
nested invocation publishes the value through return data after all child calls
finish, because a subsequent CPI would clear earlier return data.

The program emits `v42:` trace messages for entry, CPI, clock access, results,
and failures. Result messages distinguish `account0` from `return_data` routing.

A `TRANSFER` instruction instead reads an exact little-endian `i64` delta from
its data and applies it to two distinct writable accounts owned by this program.
Account 0 receives the negated delta and account 1 receives the delta, updating
both lamports and the little-endian `i64` calculator value in their first eight
data bytes. Both updates use wrapping two's-complement arithmetic, so a negative
delta reverses the transfer direction. Neither account needs to sign. Transfer
does not run the evaluator or publish return data.
