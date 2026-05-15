//! Program deployment functionality.

use {
    crate::{
        invoke_context::InvokeContext,
        loaded_programs::{ProgramCacheEntry, ProgramCacheForTxBatch, ProgramRuntimeEnvironment},
    },
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    solana_sbpf::{
        elf::{ElfError, Executable},
        program::BuiltinProgram,
        verifier::RequisiteVerifier,
    },
    solana_svm_log_collector::{LogCollector, ic_logger_msg},
    solana_svm_type_overrides::sync::Arc,
    std::{cell::RefCell, rc::Rc},
};

fn morph_into_deployment_environment_v1<'a>(
    from: Arc<BuiltinProgram<InvokeContext<'a, 'a>>>,
) -> Result<BuiltinProgram<InvokeContext<'a, 'a>>, ElfError> {
    let mut config = from.get_config().clone();
    config.reject_broken_elfs = true;
    // Once the tests are being build using a toolchain which supports the newer SBPF versions,
    // the deployment of older versions will be disabled:
    // config.enabled_sbpf_versions =
    //     *config.enabled_sbpf_versions.end()..=*config.enabled_sbpf_versions.end();

    let mut result = BuiltinProgram::new_loader(config);

    for (_key, (name, value)) in from.get_function_registry().iter() {
        // Deployment of programs with sol_alloc_free is disabled. So do not register the syscall.
        if name != *b"sol_alloc_free_" {
            result.register_function(unsafe { std::str::from_utf8_unchecked(name) }, value)?;
        }
    }

    Ok(result)
}

/// Directly deploy a program using a provided invoke context.
/// This function should only be invoked from the runtime, since it does not
/// provide any account loads or checks.
#[allow(clippy::too_many_arguments)]
pub fn deploy_program(
    log_collector: Option<Rc<RefCell<LogCollector>>>,
    program_cache_for_tx_batch: &mut ProgramCacheForTxBatch,
    program_runtime_environment: ProgramRuntimeEnvironment,
    program_id: &Pubkey,
    programdata: &[u8],
) -> Result<(), InstructionError> {
    let deployment_program_runtime_environment = morph_into_deployment_environment_v1(Arc::clone(
        &*program_runtime_environment,
    ))
    .map_err(|e| {
        ic_logger_msg!(log_collector, "Failed to register syscalls: {}", e);
        InstructionError::ProgramEnvironmentSetupFailure
    })?;
    // Verify using stricter deployment_program_runtime_environment
    let executable = Executable::<InvokeContext>::load(
        programdata,
        Arc::new(deployment_program_runtime_environment),
    )
    .map_err(|err| {
        ic_logger_msg!(log_collector, "{}", err);
        InstructionError::InvalidAccountData
    })?;
    executable.verify::<RequisiteVerifier>().map_err(|err| {
        ic_logger_msg!(log_collector, "{}", err);
        InstructionError::InvalidAccountData
    })?;
    // Reload but with program_runtime_environment
    let executor = unsafe {
        // SAFETY: The executable has been verified just above.
        ProgramCacheEntry::reload(program_runtime_environment, programdata)
    }
    .map_err(|err| {
        ic_logger_msg!(log_collector, "{}", err);
        InstructionError::InvalidAccountData
    })?;
    program_cache_for_tx_batch.store_modified_entry(*program_id, Arc::new(executor));
    Ok(())
}

#[macro_export]
macro_rules! deploy_program {
    ($invoke_context:expr, $program_id:expr, $loader_key:expr, $account_size:expr, $programdata:expr, $deployment_slot:expr $(,)?) => {
        assert_eq!(
            $deployment_slot,
            $invoke_context.program_cache_for_tx_batch.slot()
        );
        #[cfg(feature = "metrics")]
        let mut load_program_metrics = $crate::loaded_programs::LoadProgramMetrics::default();
        $crate::deploy::deploy_program(
            $invoke_context.get_log_collector(),
            #[cfg(feature = "metrics")]
            &mut load_program_metrics,
            $invoke_context.program_cache_for_tx_batch,
            $invoke_context
                .get_program_runtime_environments_for_deployment()
                .get_env_for_deployment()
                .clone(),
            $program_id,
            $loader_key,
            $account_size,
            $programdata,
            $deployment_slot,
        )?;
        #[cfg(feature = "metrics")]
        load_program_metrics.submit_datapoint(&mut $invoke_context.timings);
    };
}
