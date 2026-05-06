//! Signed value transfers between calculator accounts.

use solana_account_info::AccountInfo;
use solana_msg::msg;
use solana_program_error::{ProgramError, ProgramResult};

use crate::calculator::{read_account_value, write_account_value};

/// Applies the encoded signed delta to the lamports and calculator values of
/// two distinct accounts. Both accounts must be owned by this program and
/// writable.
pub(crate) fn process(accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let [from, to, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if from.key == to.key {
        return Err(ProgramError::InvalidArgument);
    }
    let delta = data
        .get(1..)
        .map(TryInto::<[u8; 8]>::try_into)
        .ok_or(ProgramError::InvalidInstructionData)?
        .map(i64::from_le_bytes)
        .map_err(|_| ProgramError::InvalidInstructionData)?;

    msg!("v42: transfer delta={}", delta);
    apply_delta(from, delta.wrapping_neg())?;
    apply_delta(to, delta)
}

/// Adds the same two's-complement delta to an account's balance and stored
/// calculator value.
fn apply_delta(account: &AccountInfo, delta: i64) -> ProgramResult {
    let mut lamports = account.try_borrow_mut_lamports()?;
    **lamports = (**lamports).wrapping_add(delta as u64);
    drop(lamports);

    let mut data = account.try_borrow_mut_data()?;
    let result = read_account_value(&data)?.wrapping_add(delta);
    write_account_value(&mut data, result)
}
