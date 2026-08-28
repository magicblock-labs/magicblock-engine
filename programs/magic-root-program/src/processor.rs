use magic_root_interface::MagicRootInstruction;
use nucleus::tls::AUTHORITY;
use solana_instruction::TRANSACTION_LEVEL_STACK_HEIGHT;
use solana_instruction_error::InstructionError;
use solana_program_runtime::{
    invoke_context::InvokeContext, loaded_programs::ProgramCacheEntryType,
};
use solana_svm_log_collector::ic_msg;

use crate::{TARGET_ACCOUNT_IDX, account, post_finalize};

/// Authorizes and decodes a MagicRoot instruction before validating and
/// dispatching its target operation.
pub(crate) fn process(ctx: &mut InvokeContext<'_, '_>) -> Result<(), InstructionError> {
    authorize(ctx)?;
    let instruction = decode(ctx)?;
    dispatch(ctx, instruction)
}

/// Rejects recursive or non-builtin CPI callers and callers other than [`AUTHORITY`].
///
/// MagicRoot is engine-internal: it must run either at the transaction level or
/// directly below a builtin program. The transaction must be signed by the
/// authority in either case. Both conditions are checked before any account is
/// touched.
///
/// Authorizing on account 0's key alone is sound because the engine verifies the
/// fee-payer signature at its transaction ingress (`engine::transaction`), so a
/// transaction reaching execution with `AUTHORITY` as account 0 was signed by it.
pub(crate) fn authorize(ctx: &InvokeContext<'_, '_>) -> Result<(), InstructionError> {
    let height = ctx.get_stack_height();
    if height != TRANSACTION_LEVEL_STACK_HEIGHT {
        let instruction = ctx.transaction_context.get_current_instruction_context()?;
        let caller = ctx
            .transaction_context
            .get_instruction_context_at_index_in_trace(instruction.get_index_of_caller())?;
        let caller_id = caller.get_program_key()?;
        if caller_id == &magic_root_interface::ID {
            ic_msg!(ctx, "MagicRoot: recursive caller");
            return Err(InstructionError::CallDepth);
        }
        let is_builtin = ctx
            .program_cache_for_tx_batch
            .find(caller_id)
            .filter(|e| matches!(e.program, ProgramCacheEntryType::Builtin(_)))
            .is_some();
        if !is_builtin {
            ic_msg!(ctx, "MagicRoot: non-builtin caller {}", caller_id);
            return Err(InstructionError::CallDepth);
        }
    }
    let signer = *ctx.transaction_context.get_key_of_account_at_index(0)?;
    if signer != AUTHORITY.get() {
        ic_msg!(ctx, "MagicRoot: unauthorized signer {}", signer);
        return Err(InstructionError::MissingRequiredSignature);
    }
    Ok(())
}

/// Dispatches the decoded instruction to the matching handler.
fn dispatch(
    ctx: &mut InvokeContext<'_, '_>,
    instruction: MagicRootInstruction,
) -> Result<(), InstructionError> {
    let target = ctx
        .transaction_context
        .get_current_instruction_context()?
        .get_index_of_instruction_account_in_transaction(TARGET_ACCOUNT_IDX)?;
    match instruction {
        MagicRootInstruction::Patch(patch) => account::patch(ctx, target, patch),
        MagicRootInstruction::Finalize(flags) => account::finalize(ctx, target, flags),
        MagicRootInstruction::Delete => account::delete(ctx, target),
        MagicRootInstruction::PostFinalize(actions) => post_finalize::process(ctx, actions),
        MagicRootInstruction::Prepare => account::prepare(ctx, target),
    }
}

/// Decodes the current instruction's data into a [`MagicRootInstruction`].
fn decode(ctx: &InvokeContext<'_, '_>) -> Result<MagicRootInstruction, InstructionError> {
    let instruction = ctx.transaction_context.get_current_instruction_context()?;
    wincode::deserialize_exact(instruction.get_instruction_data()).map_err(|_| {
        ic_msg!(ctx, "MagicRoot: malformed instruction data");
        InstructionError::InvalidInstructionData
    })
}
