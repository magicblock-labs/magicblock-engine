use magic_root_interface::PostFinalize;
use solana_instruction_error::InstructionError;
use solana_program_runtime::invoke_context::InvokeContext;
use solana_svm_log_collector::ic_msg;

/// Runs the post-finalize follow-up instructions, invoking each action via CPI
/// while vouching for exactly the signers that action itself declares, then
/// rejects the whole instruction if an account exposed as writable did not end
/// in a mode this engine may mutate.
pub(crate) fn process(
    ctx: &mut InvokeContext<'_, '_>,
    post_finalize: PostFinalize,
) -> Result<(), InstructionError> {
    ic_msg!(
        ctx,
        "MagicRoot: post-finalize {} action(s)",
        post_finalize.actions.len()
    );
    for action in post_finalize.actions {
        let signers: Vec<_> = action
            .accounts
            .iter()
            .filter_map(|meta| meta.is_signer.then_some(meta.pubkey))
            .collect();
        ctx.native_invoke_as(post_finalize.source_program, action, &signers)?;
    }

    let instruction = ctx.transaction_context.get_current_instruction_context()?;
    let count = instruction.get_number_of_instruction_accounts();
    for i in 0..count {
        let index = instruction.get_index_of_instruction_account_in_transaction(i)?;
        let account = ctx.transaction_context.accounts().try_borrow(index)?;
        if instruction.is_instruction_account_writable(i)? && !account.mutable() {
            ic_msg!(
                ctx,
                "MagicRoot: post-finalize rejected immutable writable account: {}",
                ctx.transaction_context.get_key_of_account_at_index(index)?
            );
            return Err(InstructionError::Immutable);
        }
    }
    Ok(())
}
