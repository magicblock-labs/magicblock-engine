# `v42-calculator-interface`

This crate defines the v42 calculator program id, instruction wire constants,
and optional off-chain builders. The SBF program depends on the wire definitions
with the default `builder` feature disabled.

`Expr` produces postfix instruction data from signed `i64` literals, account
operands, the `Clock` sysvar, arithmetic operators, and recursive self-CPI
subexpressions. Expression composition concatenates existing postfix byte
streams.

`Expr::compose` builds an instruction with the writable output at account zero,
read-only operands after it, and the calculator program id last for recursive
CPI. `Expr::acc` indexes the full instruction account list, so operand indexes
start at one and remain stable across nested calls.

`builder::transfer` applies a signed delta between distinct writable v42 accounts
at indexes 0 and 1. Its data is `TRANSFER` followed by a little-endian `i64`;
positive values move lamports and calculator value from account 0 to account 1,
while negative values reverse the direction.
