//! Postfix calculator evaluation and result routing.

use solana_account_info::AccountInfo;
use solana_cpi::{get_return_data, invoke, set_return_data};
use solana_instruction::{AccountMeta, Instruction, TRANSACTION_LEVEL_STACK_HEIGHT};
use solana_msg::msg;
use solana_program_error::{ProgramError, ProgramResult};
use solana_sysvar::clock::Clock;
use solana_sysvar::Sysvar;
use v42_calculator_interface::{opcodes::*, ID};

use crate::error::CalcError;

/// Evaluates the RPN program in `data` and routes the result: a **top-level**
/// invocation writes it (LE `i64`) into the output account; a **CPI**
/// invocation returns it through return data. The two are told apart by the
/// stack height.
///
/// A nested call must publish its result *after* its own child `CALL`s complete,
/// because every CPI clears the return-data register — which is exactly what
/// happens here, since the result is emitted only once evaluation is done.
pub(crate) fn process(accounts: &[AccountInfo], data: &[u8], height: usize) -> ProgramResult {
    let result = eval(accounts, data)?;
    if height == TRANSACTION_LEVEL_STACK_HEIGHT {
        msg!("v42: result={} -> account0", result);
        let output = accounts.first().ok_or(CalcError::MissingOutput)?;
        write_account_value(&mut output.try_borrow_mut_data()?, result)
    } else {
        msg!("v42: result={} -> return_data", result);
        set_return_data(&result.to_le_bytes());
        Ok(())
    }
}

/// Runs the token stream against an operand stack and returns the single value
/// left at the end.
fn eval(accounts: &[AccountInfo], data: &[u8]) -> Result<i64, ProgramError> {
    let mut cursor = Cursor(data);
    let mut stack = Stack::new();
    while !cursor.is_empty() {
        match cursor.u8()? {
            PUSH_LIT => stack.push(cursor.i64()?)?,
            PUSH_ACC => stack.push(account_i64(accounts, cursor.u8()?)?)?,
            PUSH_CLOCK => stack.push(clock_ts()?)?,
            op @ (ADD | SUB | MUL | DIV) => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(arithmetic(op, a, b)?)?;
            }
            CALL => {
                let len = cursor.u16()? as usize;
                let nested = cursor.take(len)?;
                stack.push(call(accounts, nested)?)?;
            }
            _ => return Err(CalcError::BadOpcode.into()),
        }
    }
    stack.into_result().map_err(Into::into)
}

fn arithmetic(op: u8, a: i64, b: i64) -> Result<i64, CalcError> {
    match op {
        ADD => a.checked_add(b).ok_or(CalcError::Arithmetic),
        SUB => a.checked_sub(b).ok_or(CalcError::Arithmetic),
        MUL => a.checked_mul(b).ok_or(CalcError::Arithmetic),
        DIV if b == 0 => Err(CalcError::DivByZero),
        DIV => a.checked_div(b).ok_or(CalcError::Arithmetic),
        _ => Err(CalcError::BadOpcode),
    }
}

/// Reads the calculator value from the first eight account data bytes.
pub(crate) fn read_account_value(bytes: &[u8]) -> Result<i64, ProgramError> {
    read_head_i64(bytes, CalcError::ShortAccount).map_err(Into::into)
}

/// Writes the calculator value to the first eight account data bytes.
pub(crate) fn write_account_value(bytes: &mut [u8], value: i64) -> ProgramResult {
    bytes
        .get_mut(..8)
        .ok_or(CalcError::ShortAccount)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// Decodes the `i64` LE stored in the first 8 bytes of `bytes`, reporting `short`
/// if there aren't that many.
fn read_head_i64(bytes: &[u8], short: CalcError) -> Result<i64, CalcError> {
    let head = bytes.get(..8).ok_or(short)?;
    Ok(i64::from_le_bytes(head.try_into().expect("length checked")))
}

/// Reads the `i64` LE operand from the first 8 data bytes of an instruction
/// account (borrowed, not copied).
fn account_i64(accounts: &[AccountInfo], index: u8) -> Result<i64, ProgramError> {
    let account = accounts.get(index as usize).ok_or(CalcError::BadAccountIndex)?;
    read_account_value(&account.try_borrow_data()?)
}

fn clock_ts() -> Result<i64, ProgramError> {
    let ts = Clock::get()?.unix_timestamp;
    msg!("v42: clock.ts={}", ts);
    Ok(ts)
}

/// Evaluates `nested` by invoking this same program, forwarding every account
/// unchanged so `PUSH_ACC` indices are identical at every recursion depth, and
/// returns the callee's return-data `i64`.
fn call(accounts: &[AccountInfo], nested: &[u8]) -> Result<i64, ProgramError> {
    let metas = accounts
        .iter()
        .map(|a| AccountMeta {
            pubkey: *a.key,
            is_signer: a.is_signer,
            is_writable: a.is_writable,
        })
        .collect();
    let instruction = Instruction {
        program_id: ID,
        accounts: metas,
        data: nested.to_vec(),
    };
    msg!("v42: cpi len={}", nested.len());
    invoke(&instruction, accounts)?;

    let (_, bytes) = get_return_data().ok_or(CalcError::MissingReturnData)?;
    Ok(read_head_i64(&bytes, CalcError::ShortReturnData)?)
}

/// A forward cursor over the instruction byte stream. Every read is bounds-
/// checked and advances the cursor; operands are read in place, never copied.
struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn u8(&mut self) -> Result<u8, CalcError> {
        let (&byte, rest) = self.0.split_first().ok_or(CalcError::Truncated)?;
        self.0 = rest;
        Ok(byte)
    }

    fn u16(&mut self) -> Result<u16, CalcError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, CalcError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], CalcError> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CalcError> {
        if self.0.len() < len {
            return Err(CalcError::Truncated);
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(head)
    }
}

/// Fixed-capacity operand stack — deep enough for any expression a test would
/// build, and allocation-free.
struct Stack {
    slots: [i64; 64],
    len: usize,
}

impl Stack {
    fn new() -> Self {
        Self { slots: [0; 64], len: 0 }
    }

    fn push(&mut self, value: i64) -> Result<(), CalcError> {
        *self.slots.get_mut(self.len).ok_or(CalcError::StackOverflow)? = value;
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Result<i64, CalcError> {
        self.len = self.len.checked_sub(1).ok_or(CalcError::StackUnderflow)?;
        Ok(self.slots[self.len])
    }

    /// The single value a well-formed program leaves behind.
    fn into_result(self) -> Result<i64, CalcError> {
        match self.len {
            1 => Ok(self.slots[0]),
            _ => Err(CalcError::UnbalancedProgram),
        }
    }
}
