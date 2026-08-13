#![doc = include_str!("../README.md")]

mod calculator;
mod error;
mod transfer;

use solana_account_info::AccountInfo;
use solana_instruction::syscalls::get_stack_height;
use solana_msg::msg;
use solana_program_entrypoint::entrypoint;
use solana_program_error::ProgramResult;
use solana_pubkey::Pubkey;
use v42_calculator_interface::opcodes::TRANSFER;

entrypoint!(process_instruction);

fn process_instruction(_: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let height = get_stack_height();
    msg!("v42: enter height={} len={}", height, data.len());
    if data.first() == Some(&TRANSFER) {
        transfer::process(accounts, data)
    } else {
        calculator::process(accounts, data, height)
    }
}
