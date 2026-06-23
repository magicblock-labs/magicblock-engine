use {
    crate::{entrypoint::MagicRootEntrypoint, processor::authorize},
    nucleus::tls::AUTHORITY,
    solana_account::AccountSharedData,
    solana_instruction_error::InstructionError,
    solana_program_runtime::{
        loaded_programs::{ProgramCacheEntry, ProgramCacheEntryType},
        solana_sbpf::program::BuiltinFunctionDefinition,
        with_mock_invoke_context,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::native_loader,
    std::sync::Arc,
};

#[derive(Clone, Copy)]
enum Caller {
    Builtin,
    User,
}

fn authorize_chain(
    authority: Pubkey,
    signer: Pubkey,
    callers: &[Caller],
) -> Result<(), InstructionError> {
    AUTHORITY.set(authority);

    let caller_ids = callers.iter().map(|_| Pubkey::new_unique()).collect::<Vec<_>>();
    let mut accounts = Vec::with_capacity(caller_ids.len().saturating_add(2));
    accounts.push((signer, AccountSharedData::default()));
    accounts.extend(
        caller_ids
            .iter()
            .map(|id| (*id, AccountSharedData::new(1, 0, &native_loader::ID))),
    );
    accounts.push((
        magic_root_interface::ID,
        AccountSharedData::new(1, 0, &native_loader::ID),
    ));

    with_mock_invoke_context!(ctx, transaction_context, accounts);
    let mut cache = ProgramCacheForTxBatch::default();
    let environments = ProgramRuntimeEnvironments::default();
    for (&id, caller) in caller_ids.iter().zip(callers) {
        let entry = match caller {
            Caller::Builtin => ProgramCacheEntry::new_builtin((
                MagicRootEntrypoint::vm,
                MagicRootEntrypoint::codegen,
            )),
            Caller::User => ProgramCacheEntry {
                program: ProgramCacheEntryType::Unloaded(
                    environments.get_env_for_execution().clone(),
                ),
            },
        };
        cache.replenish(id, Arc::new(entry));
    }
    ctx.program_cache_for_tx_batch = &mut cache;

    ctx.transaction_context
        .configure_top_level_instruction_for_tests(1, Vec::new(), Vec::new())?;
    ctx.push()?;
    for program_index in 2..=callers.len().saturating_add(1) {
        ctx.transaction_context.configure_next_cpi_for_tests(
            program_index as u16,
            Vec::new(),
            Vec::new(),
        )?;
        ctx.push()?;
    }

    authorize(&ctx)
}

#[test]
fn authorizes_top_level_and_builtin_cpi() {
    let authority = Pubkey::new_unique();
    assert_eq!(authorize_chain(authority, authority, &[]), Ok(()));
    assert_eq!(
        authorize_chain(authority, authority, &[Caller::Builtin]),
        Ok(())
    );
    assert_eq!(
        authorize_chain(authority, authority, &[Caller::Builtin, Caller::Builtin],),
        Ok(())
    );
}

#[test]
fn rejects_a_non_builtin_caller() {
    let authority = Pubkey::new_unique();
    assert_eq!(
        authorize_chain(authority, authority, &[Caller::User]),
        Err(InstructionError::CallDepth)
    );
}

#[test]
fn authorizes_an_immediate_builtin_caller() {
    let authority = Pubkey::new_unique();
    assert_eq!(
        authorize_chain(authority, authority, &[Caller::User, Caller::Builtin]),
        Ok(())
    );
}

#[test]
fn rejects_the_wrong_authority() {
    let authority = Pubkey::new_unique();
    assert_eq!(
        authorize_chain(authority, Pubkey::new_unique(), &[Caller::Builtin]),
        Err(InstructionError::MissingRequiredSignature)
    );
}
