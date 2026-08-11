use solana_account::{AccountMode, AccountSharedData, DirtyMarkers};
use solana_instruction::Instruction;
use solana_instruction_error::InstructionError;
use solana_program_runtime::invoke_context::InvokeContext;
use solana_svm_log_collector::ic_msg;

/// Runs the post-finalize follow-up instructions, invoking each action via CPI
/// while vouching for exactly the signers that action itself declares, then
/// rejects the whole instruction if an action left an account modified that this
/// engine is not authoritative for.
pub(crate) fn process(
    ctx: &mut InvokeContext<'_, '_>,
    actions: Vec<Instruction>,
) -> Result<(), InstructionError> {
    ic_msg!(ctx, "MagicRoot: post-finalize {} action(s)", actions.len());
    for action in actions {
        let signers: Vec<_> = action
            .accounts
            .iter()
            .filter_map(|meta| meta.is_signer.then_some(meta.pubkey))
            .collect();
        ctx.native_invoke(action, &signers)?;
    }

    let instruction = ctx.transaction_context.get_current_instruction_context()?;
    let count = instruction.get_number_of_instruction_accounts();
    for i in 0..count {
        let index = instruction.get_index_of_instruction_account_in_transaction(i)?;
        let account = ctx.transaction_context.accounts().try_borrow(index)?;
        if account.dirty() && !writable(&account) {
            ic_msg!(
                ctx,
                "MagicRoot: post-finalize modified immutable account {}",
                ctx.transaction_context.get_key_of_account_at_index(index)?
            );
            return Err(InstructionError::Immutable);
        }
    }
    Ok(())
}

/// Returns whether an action may leave the account modified.
///
/// Kept in step with the SVM's `access_permissions`: mutable modes are writable
/// directly, and `Transient`/`Closed` are writable only when the mode dirty
/// marker records a lifecycle transition in the current transaction.
fn writable(account: &AccountSharedData) -> bool {
    if account.mutable() {
        return true;
    }
    matches!(account.mode(), AccountMode::Transient | AccountMode::Closed)
        && account.markers().contains(DirtyMarkers::MODE)
}
