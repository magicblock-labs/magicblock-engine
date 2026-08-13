//! Opcodes for the v42-calculator RPN byte stream — the single source of truth
//! for the wire format, shared by the off-chain `Expr` builder and the on-chain
//! evaluator so the two can never drift. Adding an operation is one constant
//! here plus one match arm in the program.
//!
//! A program is a flat sequence of tokens evaluated left-to-right against a
//! `i64` stack: `PUSH_*` tokens push one value, the arithmetic tokens pop two
//! and push one, and `CALL` evaluates a nested program through a self-CPI and
//! pushes its result. Exactly one value must remain when the stream ends.

/// Push an immediate `i64`; 8 little-endian bytes follow.
pub const PUSH_LIT: u8 = 0x00;
/// Push the `i64` LE held in the first 8 data bytes of an instruction account;
/// a 1-byte account index follows.
pub const PUSH_ACC: u8 = 0x01;
/// Push the current clock unix timestamp.
pub const PUSH_CLOCK: u8 = 0x03;

/// Pop `b`, pop `a`, push `a + b` (checked).
pub const ADD: u8 = 0x10;
/// Pop `b`, pop `a`, push `a - b` (checked).
pub const SUB: u8 = 0x11;
/// Pop `b`, pop `a`, push `a * b` (checked).
pub const MUL: u8 = 0x12;
/// Pop `b`, pop `a`, push checked `a / b` (`b == 0` is an error).
pub const DIV: u8 = 0x13;

/// Evaluate a nested program via self-CPI and push its return-data `i64`. A
/// `u16` LE byte length follows, then that many bytes of nested program.
pub const CALL: u8 = 0x20;

/// Transfer lamports and calculator value between instruction accounts 0 and
/// 1; an exact little-endian `i64` delta follows.
pub const TRANSFER: u8 = 0x30;
