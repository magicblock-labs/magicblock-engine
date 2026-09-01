use solana_account::{
    AccountFieldPatch, AccountMode, ReadableAccount, StateFlags, WritableAccount,
};
use solana_instruction_error::InstructionError;
use solana_program_runtime::{invoke_context::InvokeContext, loaded_programs::ProgramCacheEntry};
use solana_svm_log_collector::ic_msg;
use solana_transaction_context::{IndexOfAccount, MAX_ACCOUNT_DATA_LEN};

/// Applies one bounded patch to the target account.
pub(crate) fn patch(
    ctx: &InvokeContext<'_, '_>,
    target: IndexOfAccount,
    patch: AccountFieldPatch,
) -> Result<(), InstructionError> {
    let mut account = ctx.transaction_context.accounts().try_borrow_mut(target)?;
    ic_msg!(ctx, "MagicRoot: patch {:?}", patch);
    let data_len = match &patch {
        AccountFieldPatch::DataAt { offset, data } => {
            let end = offset.checked_add(data.len()).ok_or(InstructionError::InvalidRealloc)?;
            Some(account.data().len().max(end))
        }
        AccountFieldPatch::DataLen(len) => Some(*len),
        _ => None,
    };
    if data_len.is_some_and(|len| len > MAX_ACCOUNT_DATA_LEN as usize) {
        return Err(InstructionError::InvalidRealloc);
    }
    let old = account.lamports();
    if let Err(error) = patch.apply(&mut account) {
        ic_msg!(ctx, "MagicRoot: {}", error);
        return Err(InstructionError::InvalidArgument);
    }
    let new = account.lamports();
    let mut authority = ctx.transaction_context.accounts().try_borrow_mut(0)?;
    // Balance target changes against the authority to preserve total lamports.
    if new > old {
        authority.checked_sub_lamports(new - old)?;
    } else if new < old {
        authority.checked_add_lamports(old - new)?;
    }
    Ok(())
}

/// Loads an executable target into the transaction's program cache.
pub(crate) fn finalize(
    ctx: &mut InvokeContext<'_, '_>,
    target: IndexOfAccount,
    flags: StateFlags,
) -> Result<(), InstructionError> {
    let mut account = ctx.transaction_context.accounts().try_borrow_mut(target)?;
    account.set_flags(flags);
    if !account.executable() {
        return Ok(());
    }
    let pubkey = ctx.transaction_context.get_key_of_account_at_index(target)?;
    let entry = ProgramCacheEntry::new(
        ctx.environment_config
            .program_runtime_environments_for_execution
            .get_env_for_execution()
            .clone(),
        account.data(),
    )
    .map_err(|_| {
        ic_msg!(ctx, "MagicRoot: program load failed {}", pubkey);
        InstructionError::ProgramEnvironmentSetupFailure
    })?
    .into();
    ctx.program_cache_for_tx_batch.store_modified_entry(*pubkey, entry);
    ic_msg!(ctx, "MagicRoot: finalized program load");
    Ok(())
}

/// Marks the target account closed for removal from storage.
pub(crate) fn delete(
    ctx: &mut InvokeContext<'_, '_>,
    target: IndexOfAccount,
) -> Result<(), InstructionError> {
    let mut acc = ctx.transaction_context.accounts().try_borrow_mut(target)?;

    let slot = acc.slot();
    if let Err(error) = acc.set_lifecycle(AccountMode::Closed, slot) {
        ic_msg!(ctx, "MagicRoot: {}", error);
        return Err(InstructionError::InvalidArgument);
    }
    let pubkey = *ctx.transaction_context.get_key_of_account_at_index(target)?;
    if acc.executable() {
        let entry = ProgramCacheEntry::default().into();
        ctx.program_cache_for_tx_batch.store_modified_entry(pubkey, entry);
    }
    ic_msg!(ctx, "MagicRoot: removed");
    Ok(())
}
