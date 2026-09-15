use {
    crate::{entrypoint::MagicRootEntrypoint, processor::authorize},
    magic_root_interface::{MagicRootInstruction, PostFinalize},
    nucleus::tls::AUTHORITY,
    solana_account::{
        AccountBuilder, AccountFieldPatch, AccountMode, AccountSharedData, ReadableAccount,
        WritableAccount,
    },
    solana_instruction::{AccountMeta, Instruction},
    solana_instruction_error::InstructionError,
    solana_program_runtime::{
        declare_process_instruction,
        invoke_context::InvokeContext,
        loaded_programs::{ProgramCacheEntry, ProgramCacheEntryType},
        solana_sbpf::program::BuiltinFunctionDefinition,
        with_mock_invoke_context,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::native_loader,
    solana_transaction_context::instruction_accounts::InstructionAccount,
    std::sync::Arc,
};

#[derive(Clone, Copy, Debug)]
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
        ctx.program_cache_for_tx_batch.replenish(id, Arc::new(entry));
    }

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

/// Proves only top-level authority calls pass without explicit authorization,
/// including rejection of non-builtin callers and builtin intermediaries.
#[test]
fn ordinary_authorization() {
    use Caller::{Builtin, User};
    use InstructionError::{CallDepth, MissingRequiredSignature};

    let authority = Pubkey::new_unique();
    for (callers, signer, expected) in [
        (&[] as &[Caller], authority, Ok(())),
        (&[], Pubkey::new_unique(), Err(MissingRequiredSignature)),
        (&[Builtin], authority, Err(MissingRequiredSignature)),
        (
            &[Builtin, Builtin],
            authority,
            Err(MissingRequiredSignature),
        ),
        (&[User], authority, Err(CallDepth)),
        (&[User, Builtin], authority, Err(MissingRequiredSignature)),
    ] {
        assert_eq!(
            authorize_chain(authority, signer, callers),
            expected,
            "callers: {callers:?}, signer: {signer}"
        );
    }
}

const CALLER: Pubkey = Pubkey::new_from_array([2; 32]);
const PROBE: Pubkey = Pubkey::new_from_array([3; 32]);

// A native probe checks frame identity directly, including nested scopes, without
// exposing a test-only authorization setter in the runtime's public API.
declare_process_instruction!(Probe, 1, |ctx| {
    let instruction = ctx.transaction_context.get_current_instruction_context()?;
    match instruction.get_instruction_data() {
        [0] => assert!(!ctx.is_magic_root_authorized()),
        [1] => {
            assert!(ctx.is_magic_root_authorized());
            let ordinary = Instruction::new_with_bytes(PROBE, &[0], vec![]);
            ctx.native_invoke_signed(ordinary.clone(), &[])?;
            ctx.native_invoke_as(CALLER, ordinary, &[])?;
            for (data, expected) in [(3, Ok(())), (2, Err(InstructionError::InvalidArgument))] {
                let instruction = Instruction::new_with_bytes(PROBE, &[data], vec![]);
                assert_eq!(ctx.native_invoke_magic_root(instruction), expected);
                // Success and failure both restore, rather than erase, the parent's scope.
                assert!(ctx.is_magic_root_authorized());
            }
        }
        [2] => {
            assert!(ctx.is_magic_root_authorized());
            return Err(InstructionError::InvalidArgument);
        }
        [3] => assert!(ctx.is_magic_root_authorized()),
        _ => return Err(InstructionError::InvalidInstructionData),
    }
    Ok(())
});

fn with_cpi(writable: bool, run: impl FnOnce(&mut InvokeContext<'_, '_>, Pubkey)) {
    let authority = Pubkey::new_unique();
    let target = Pubkey::new_unique();
    AUTHORITY.set(authority);
    let mut accounts = vec![
        (
            authority,
            AccountBuilder::default()
                .lamports(10_000_000)
                .mode(AccountMode::Ephemeral)
                .build(),
        ),
        (target, AccountSharedData::default()),
    ];
    for id in [CALLER, magic_root_interface::ID, PROBE] {
        let mut account = AccountSharedData::new(1, 0, &native_loader::ID);
        account.set_executable(true);
        accounts.push((id, account));
    }
    with_mock_invoke_context!(ctx, transaction_context, accounts);
    let probe = Arc::new(ProgramCacheEntry::new_builtin((Probe::vm, Probe::codegen)));
    for (id, entry) in [
        (CALLER, probe.clone()),
        (
            magic_root_interface::ID,
            Arc::new(ProgramCacheEntry::new_builtin((
                MagicRootEntrypoint::vm,
                MagicRootEntrypoint::codegen,
            ))),
        ),
        (PROBE, probe),
    ] {
        ctx.program_cache_for_tx_batch.replenish(id, entry);
    }
    ctx.transaction_context
        .configure_top_level_instruction_for_tests(
            2,
            vec![
                InstructionAccount::new(0, true, true),
                InstructionAccount::new(1, false, writable),
                InstructionAccount::new(2, false, false),
                InstructionAccount::new(3, false, false),
                InstructionAccount::new(4, false, false),
            ],
            vec![],
        )
        .unwrap();
    ctx.push().unwrap();
    run(&mut ctx, target);
}

/// Proves both ordinary native APIs reject forwarded MagicRoot operations without touching accounts.
#[test]
fn rejects_forwarded_magic_root_instructions() {
    with_cpi(true, |ctx, target| {
        let instruction = MagicRootInstruction::Delete.compose(target).unwrap();
        for (api, result) in [
            ("signed", ctx.native_invoke_signed(instruction.clone(), &[])),
            ("attributed", ctx.native_invoke_as(CALLER, instruction, &[])),
        ] {
            assert_eq!(
                result,
                Err(InstructionError::MissingRequiredSignature),
                "{api}"
            );
        }
        assert_eq!(
            ctx.transaction_context.accounts().try_borrow(1).unwrap().mode(),
            AccountMode::Placeholder
        );
    });
}

/// Proves explicit internal creation preserves account data, lifecycle, and balanced authority funding.
#[test]
fn explicit_invocation_creates_an_account() {
    with_cpi(true, |ctx, target| {
        let account = AccountBuilder::default()
            .lamports(2_000_000)
            .owner(CALLER)
            .mode(AccountMode::Ephemeral)
            .slot(1)
            .data(vec![7, 8])
            .build();
        for instruction in MagicRootInstruction::compose_account(target, account).unwrap() {
            ctx.native_invoke_magic_root(instruction).unwrap();
            assert!(!ctx.is_magic_root_authorized());
        }
        let target = ctx.transaction_context.accounts().try_borrow(1).unwrap();
        assert_eq!(
            (
                target.lamports(),
                target.owner(),
                target.mode(),
                target.slot(),
                target.data()
            ),
            (2_000_000, &CALLER, AccountMode::Ephemeral, 1, &[7, 8][..])
        );
        assert_eq!(
            ctx.transaction_context.accounts().try_borrow(0).unwrap().lamports(),
            8_000_000
        );
    });
}

/// Proves authorization restores nested scopes and excludes descendants,
/// later siblings, and PostFinalize actions.
#[test]
fn authorization_is_scoped_to_the_exact_child() {
    with_cpi(true, |ctx, target| {
        let nested =
            Instruction::new_with_bytes(PROBE, &[1], vec![AccountMeta::new_readonly(PROBE, false)]);
        ctx.native_invoke_magic_root(nested).unwrap();
        let failing = Instruction::new_with_bytes(PROBE, &[2], vec![]);
        assert_eq!(
            ctx.native_invoke_magic_root(failing),
            Err(InstructionError::InvalidArgument)
        );
        assert!(!ctx.is_magic_root_authorized());
        ctx.native_invoke_signed(Instruction::new_with_bytes(PROBE, &[0], vec![]), &[])
            .unwrap();
        let post = PostFinalize {
            source_program: CALLER,
            actions: vec![Instruction::new_with_bytes(PROBE, &[0], vec![])],
        };
        let mut instruction = MagicRootInstruction::PostFinalize(post).compose(target).unwrap();
        // This test observes invocation scope, not the independent final mutability guard.
        instruction.accounts[0].is_writable = false;
        ctx.native_invoke_magic_root(instruction).unwrap();
    });
}

/// Proves CPI preparation and frame-push failures cannot authorize a later ordinary invocation.
#[test]
fn failed_invocations_do_not_leak_authorization() {
    with_cpi(true, |ctx, target| {
        let instruction = MagicRootInstruction::Delete.compose(target).unwrap();
        let mut missing = instruction.clone();
        missing.accounts[0].pubkey = Pubkey::new_unique();
        assert_eq!(
            ctx.native_invoke_magic_root(missing),
            Err(InstructionError::MissingAccount)
        );

        // Force push to fail before consuming the prepared trace index. The next
        // ordinary call reuses that index, detecting a stale authorization marker.
        ctx.transaction_context.accounts().try_borrow_mut(0).unwrap().set_owner(CALLER);
        ctx.transaction_context
            .get_current_instruction_context()
            .unwrap()
            .try_borrow_instruction_account(0)
            .unwrap()
            .checked_sub_lamports(1)
            .unwrap();
        assert_eq!(
            ctx.native_invoke_magic_root(instruction.clone()),
            Err(InstructionError::UnbalancedInstruction)
        );
        ctx.transaction_context
            .get_current_instruction_context()
            .unwrap()
            .try_borrow_instruction_account(0)
            .unwrap()
            .checked_add_lamports(1)
            .unwrap();
        assert_eq!(
            ctx.native_invoke_signed(instruction, &[]),
            Err(InstructionError::MissingRequiredSignature)
        );
    });
}

/// Proves explicit authorization does not bypass writable, signer, authority, or lifecycle checks.
#[test]
fn explicit_invocation_preserves_account_protections() {
    for writable in [false, true] {
        with_cpi(writable, |ctx, target| {
            let mut instruction = MagicRootInstruction::Delete.compose(target).unwrap();
            instruction.accounts[0].is_signer = writable;
            assert_eq!(
                ctx.native_invoke_magic_root(instruction),
                Err(InstructionError::PrivilegeEscalation)
            );
        });
    }
    with_cpi(true, |ctx, target| {
        let invalid = MagicRootInstruction::Patch(AccountFieldPatch::Lifecycle {
            mode: AccountMode::Transient,
            slot: 1,
        })
        .compose(target)
        .unwrap();
        assert_eq!(
            ctx.native_invoke_magic_root(invalid),
            Err(InstructionError::InvalidArgument)
        );
        AUTHORITY.set(Pubkey::new_unique());
        assert_eq!(
            ctx.native_invoke_magic_root(MagicRootInstruction::Delete.compose(target).unwrap()),
            Err(InstructionError::MissingRequiredSignature)
        );
    });
}

/// Proves explicit invocation still rejects non-builtin and recursive MagicRoot callers.
#[test]
fn explicit_invocation_preserves_caller_protections() {
    with_cpi(true, |ctx, target| {
        ctx.program_cache_for_tx_batch
            .replenish(CALLER, Arc::new(ProgramCacheEntry::default()));
        assert_eq!(
            ctx.native_invoke_magic_root(MagicRootInstruction::Delete.compose(target).unwrap()),
            Err(InstructionError::CallDepth)
        );
    });
    with_cpi(true, |ctx, target| {
        ctx.transaction_context
            .configure_next_cpi_for_tests(
                3,
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(3, false, false),
                ],
                vec![],
            )
            .unwrap();
        ctx.push().unwrap();
        assert_eq!(
            ctx.native_invoke_magic_root(MagicRootInstruction::Delete.compose(target).unwrap()),
            Err(InstructionError::CallDepth)
        );
    });
}
