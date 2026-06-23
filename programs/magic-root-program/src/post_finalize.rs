use solana_instruction::Instruction;
use solana_instruction_error::InstructionError;
use solana_program_runtime::invoke_context::InvokeContext;
use solana_svm_log_collector::ic_msg;

/// Runs the post-finalize follow-up instructions: rejects any writable
/// instruction account that is not mutable, then invokes each action via CPI,
/// vouching for exactly the signers that action itself declares.
pub(crate) fn process(
    ctx: &mut InvokeContext<'_, '_>,
    actions: Vec<Instruction>,
) -> Result<(), InstructionError> {
    ic_msg!(ctx, "MagicRoot: post-finalize {} action(s)", actions.len());
    let instruction = ctx.transaction_context.get_current_instruction_context()?;
    let count = instruction.get_number_of_instruction_accounts();
    for i in 0..count {
        let index = instruction.get_index_of_instruction_account_in_transaction(i)?;
        let account = ctx.transaction_context.accounts().try_borrow(index)?;
        if instruction.is_instruction_account_writable(i)? && !account.mutable() {
            ic_msg!(
                ctx,
                "MagicRoot: post-finalize rejected immutable writable account"
            );
            return Err(InstructionError::Immutable);
        }
    }
    for action in actions {
        let signers: Vec<_> = action
            .accounts
            .iter()
            .filter_map(|meta| meta.is_signer.then_some(meta.pubkey))
            .collect();
        ctx.native_invoke(action, &signers)?;
    }
    Ok(())
}
