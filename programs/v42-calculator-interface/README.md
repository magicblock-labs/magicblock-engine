# `v42-calculator-interface`

Shared instructions and optional off-chain builders for the v42 calculator test
program. The interface lets Engine tests construct arithmetic, account-read,
clock, recursive-CPI, and transfer scenarios without depending on the SBF program.

Enable the `builder` feature for expression composition and instruction helpers;
program-side consumers can use the wire definitions alone. Expressions address
the full instruction account list: account zero is the output, and read operands
start after it, including in nested calls.

Transfer instructions apply a signed delta to two distinct writable calculator
accounts, changing both lamports and stored values. Negative deltas reverse the
direction. These are test fixtures, not a production token-transfer interface.
