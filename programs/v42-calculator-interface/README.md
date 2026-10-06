# v42 calculator interface

This crate defines the wire format and off-chain builders for the
[v42 SBF fixture](../v42-calculator-program/README.md). Engine tests compose arithmetic,
account-access, Clock, CPI, and transfer scenarios here without depending on the
program's implementation. Program-side consumers share the opcode definitions only.

## Expression composition

Builders produce postfix instruction bytes directly, including nested self-CPI
expressions. The `builder` feature keeps off-chain composition separate from the
program-side wire dependency.

Account indices refer to the full instruction list: account zero is the output,
and read operands start after it. Nested calls forward the same list, preserving
indices across call depth.

Transfer instructions target two distinct writable calculator accounts. A signed
delta changes both lamports and stored values; negative deltas reverse direction.
The [program README](../v42-calculator-program/README.md) documents the fixture's
arithmetic and authorization semantics.

The [builder](src/builder.rs) defines expression composition and transfer helpers;
[opcodes](src/opcodes.rs) define the byte-level contract shared with the evaluator.
