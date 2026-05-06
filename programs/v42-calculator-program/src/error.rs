//! Stable calculator errors exposed through `ProgramError::Custom`.

use solana_msg::msg;
use solana_program_error::ProgramError;

/// Failure modes with stable discriminants for callers and tests.
#[derive(Debug)]
#[repr(u32)]
pub(crate) enum CalcError {
    Truncated = 1,
    BadOpcode,
    StackOverflow,
    StackUnderflow,
    UnbalancedProgram,
    Arithmetic,
    DivByZero,
    BadAccountIndex,
    ShortAccount,
    MissingOutput,
    MissingReturnData = 12,
    ShortReturnData,
}

impl From<CalcError> for ProgramError {
    fn from(error: CalcError) -> Self {
        msg!("v42: err {:?}", error);
        ProgramError::Custom(error as u32)
    }
}
