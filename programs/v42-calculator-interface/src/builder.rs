//! Off-chain expression builder.

use core::ops::{Add, Div, Mul, Sub};

use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use crate::{opcodes::*, ID};

/// Build a transfer between distinct v42-owned accounts. A positive `delta`
/// moves value from `from` to `to`; a negative delta reverses the direction.
/// Neither account is required to sign, but both are writable.
pub fn transfer(from: Pubkey, to: Pubkey, delta: i64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(TRANSFER);
    data.extend_from_slice(&delta.to_le_bytes());
    Instruction {
        program_id: ID,
        accounts: vec![AccountMeta::new(from, false), AccountMeta::new(to, false)],
        data,
    }
}

/// A calculator expression, built the way you'd write the math and lowered to
/// the postfix byte stream the program evaluates. Construct leaves with the
/// associated functions, combine them with `+`, `-`, `*`, `/`, and wrap any
/// subtree in [`Expr::cpi`] to force it through a recursive self-CPI (its result
/// then travels back through return data instead of being computed inline).
///
/// ```
/// use solana_pubkey::Pubkey;
/// use v42_calculator_interface::builder::Expr as E;
///
/// // (42 + (31 * 4) - 56) * 2, with the product evaluated via CPI.
/// let program = (E::lit(42) + (E::lit(31) * E::lit(4)).cpi() - E::lit(56)) * E::lit(2);
/// let ix = program.compose(Pubkey::default(), &[]);
/// assert_eq!(ix.program_id, v42_calculator_interface::ID);
/// ```
///
/// The wrapped bytes are already in postfix order, so combining expressions is
/// just concatenation — there is no intermediate tree to walk.
#[derive(Clone, Debug)]
pub struct Expr(Vec<u8>);

impl Expr {
    /// An immediate signed literal operand.
    pub fn lit(value: i64) -> Self {
        let mut bytes = Vec::with_capacity(9);
        bytes.push(PUSH_LIT);
        bytes.extend_from_slice(&value.to_le_bytes());
        Self(bytes)
    }

    /// The `i64` LE stored in instruction account `index`. Account 0 is the
    /// output account; the operands passed to [`Expr::compose`] start at 1.
    pub fn acc(index: u8) -> Self {
        Self(vec![PUSH_ACC, index])
    }

    /// The current clock unix timestamp.
    pub fn clock() -> Self {
        Self(vec![PUSH_CLOCK])
    }

    /// Evaluate this subexpression through a recursive self-CPI rather than
    /// inline. All instruction accounts are forwarded to the callee, so
    /// [`Expr::acc`] indices are unchanged at any depth.
    pub fn cpi(self) -> Self {
        let mut bytes = Vec::with_capacity(3 + self.0.len());
        bytes.push(CALL);
        bytes.extend_from_slice(&(self.0.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&self.0);
        Self(bytes)
    }

    /// The raw postfix byte stream — i.e. the instruction data.
    pub fn program(&self) -> Vec<u8> {
        self.0.clone()
    }

    /// Build the instruction: account 0 is the writable `output` the final
    /// result is written to, followed by the read-only `operands` referenced by
    /// [`Expr::acc`], followed by this program id for recursive CPI.
    pub fn compose(&self, output: Pubkey, operands: &[Pubkey]) -> Instruction {
        let mut accounts = Vec::with_capacity(2 + operands.len());
        accounts.push(AccountMeta::new(output, false));
        accounts.extend(operands.iter().map(|key| AccountMeta::new_readonly(*key, false)));
        accounts.push(AccountMeta::new_readonly(ID, false));
        Instruction {
            program_id: ID,
            accounts,
            data: self.0.clone(),
        }
    }

    fn binary(mut self, rhs: Expr, op: u8) -> Self {
        self.0.extend_from_slice(&rhs.0);
        self.0.push(op);
        self
    }
}

macro_rules! bin_op {
    ($trait:ident, $method:ident, $op:expr) => {
        impl $trait for Expr {
            type Output = Expr;
            fn $method(self, rhs: Expr) -> Expr {
                self.binary(rhs, $op)
            }
        }
    };
}

bin_op!(Add, add, ADD);
bin_op!(Sub, sub, SUB);
bin_op!(Mul, mul, MUL);
bin_op!(Div, div, DIV);
