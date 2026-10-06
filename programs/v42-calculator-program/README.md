# v42 calculator program

The v42 calculator is an SBF fixture for Engine runtime tests. It provides predictable
account reads and writes, Clock access, recursive CPI, return data, and lamport
transfers without depending on application programs. Keeper's testkit builds it
with the SBF toolchain and loads it for execution scenarios.

## Execution behavior

Postfix expressions operate on signed integers using literals, account data, and
Clock values. Subexpressions can execute through self-CPI. Top-level evaluation
writes to the output account; nested evaluation returns data to its caller. Tests
can therefore exercise account mutation and nested invocation with the same fixture.
Malformed expressions and checked-arithmetic failures return program errors.

The transfer instruction changes lamports and stored calculator values on two
distinct program-owned writable accounts. It deliberately uses wrapping arithmetic
and requires no account signatures; these are fixture semantics, not a production
transfer policy.

The [evaluator](src/calculator.rs) and [transfer handler](src/transfer.rs) implement
the behavior. Shared opcodes and off-chain builders live in the
[interface crate](../v42-calculator-interface/README.md).
