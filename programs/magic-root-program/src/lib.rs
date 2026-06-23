#![doc = include_str!("../README.md")]

mod account;
mod post_finalize;
mod processor;
#[cfg(test)]
mod tests;

use solana_instruction_error::InstructionError;
use solana_program_runtime::invoke_context::InvokeContext;

/// Instruction-account index of the account a MagicRoot instruction operates on.
pub const TARGET_ACCOUNT_IDX: u16 = 0;

/// Executes a MagicRoot instruction: authorizes the caller, decodes the
/// instruction, then dispatches it to the matching handler.
pub fn process(ctx: &mut InvokeContext<'_, '_>) -> Result<(), InstructionError> {
    processor::process(ctx)
}

#[allow(missing_docs)]
pub mod entrypoint {
    use solana_program_runtime::declare_process_instruction;
    declare_process_instruction!(MagicRootEntrypoint, 150, |ctx| { super::process(ctx) });
}
