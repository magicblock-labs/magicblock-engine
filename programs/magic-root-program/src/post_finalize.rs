use solana_account::ReadableAccount;
use solana_instruction::Instruction;
use solana_instruction_error::InstructionError;
use solana_program_runtime::invoke_context::InvokeContext;
use solana_svm_log_collector::ic_msg;

/// Runs the post-finalize follow-up instructions: rejects any writable
/// instruction account that is not mutable, then invokes each action via CPI,
/// vouching for exactly the signers that action itself declares.
///
/// "Not mutable" is about *existing* state the rollup only holds read-only.
/// An account that does not exist yet is not that, and an action is allowed to
/// create one — which is how e.g. a group receipt comes into being. Creation
/// runs through the magic program's own `CreateEphemeralAccount`, which has its
/// own rules (a delegated, funded sponsor), so nothing is waved through here
/// that is not checked there.
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
        // Exactly the shape `validate_new_ephemeral` demands of a creation
        // target: nothing to protect, because there is nothing there.
        let uncreated = account.lamports() == 0
            && account.data().is_empty()
            // The system program id is the all-zero pubkey; comparing against
            // the default avoids pulling `solana-sdk-ids` in as a runtime dep
            // for one constant.
            && *account.owner() == Default::default();
        if instruction.is_instruction_account_writable(i)? && !account.mutable() && !uncreated {
            // Name the account. A bare "something was immutable" leaves the
            // caller to guess which of a bundle's twenty-odd accounts it was,
            // and the answer is the whole diagnosis.
            let pubkey = ctx.transaction_context.get_key_of_account_at_index(index)?;
            ic_msg!(
                ctx,
                "MagicRoot: post-finalize rejected immutable writable account {}",
                pubkey
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
