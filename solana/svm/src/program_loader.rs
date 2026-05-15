use {
    solana_account::{AccountSharedData, ReadableAccount},
    solana_program_runtime::loaded_programs::{
        ProgramCacheEntry, ProgramCacheEntryType, ProgramRuntimeEnvironments,
    },
    solana_svm_type_overrides::sync::Arc,
};

/// Builds a program-cache entry from an executable account.
///
/// Invalid program data is kept as a failed-verification entry so execution
/// can report the normal program failure path.
pub fn load_program(
    environments: &ProgramRuntimeEnvironments,
    program: &AccountSharedData,
) -> Arc<ProgramCacheEntry> {
    let environment = environments.get_env_for_execution().clone();
    ProgramCacheEntry::new(environment.clone(), program.data())
        .map(Arc::new)
        .unwrap_or_else(|_| {
            ProgramCacheEntry {
                program: ProgramCacheEntryType::FailedVerification(environment),
            }
            .into()
        })
}
