# `v42-calculator-program`

An SBF test program for exercising Engine execution: account reads and writes,
clock access, checked arithmetic, recursive CPI, return data, and direct lamport
transfers. Instructions and builders come from
[the interface crate](../v42-calculator-interface/README.md).

Top-level expression evaluation writes its result to the output account; nested
evaluation returns its result to the caller. Malformed input and execution errors
return program errors rather than successful partial results.

The transfer fixture changes lamports and stored calculator values on two
program-owned writable accounts. It deliberately uses wrapping arithmetic and
does not require account signatures; it is not a production transfer program.

The program is built with the SBF toolchain and used by Keeper's test fixtures.
