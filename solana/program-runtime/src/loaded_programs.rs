use {
    crate::invoke_context::{BuiltinFunctionWithContext, InvokeContext},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_sbpf::{
        elf::Executable, program::BuiltinProgram, verifier::RequisiteVerifier, vm::Config,
    },
    solana_svm_type_overrides::sync::Arc,
    std::{
        collections::HashMap,
        fmt::{Debug, Formatter},
        hash::{Hash, Hasher},
        ops::Deref,
    },
};

#[repr(transparent)]
pub struct ProgramRuntimeEnvironment(Arc<BuiltinProgram<InvokeContext<'static, 'static>>>);

impl ProgramRuntimeEnvironment {
    pub fn from(program: BuiltinProgram<InvokeContext<'static, 'static>>) -> Self {
        Self(Arc::new(program))
    }

    /// Converts a loader reference into its transparent runtime-environment wrapper.
    ///
    /// # Safety
    ///
    /// `ProgramRuntimeEnvironment` is `repr(transparent)` over the same `Arc`
    /// type, so the reference layout is identical.
    pub unsafe fn from_ref<'a>(
        program: &'a Arc<BuiltinProgram<InvokeContext<'static, 'static>>>,
    ) -> &'a Self {
        unsafe { &*(program as *const Arc<_> as *const Self) }
    }
}

impl Clone for ProgramRuntimeEnvironment {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Debug for ProgramRuntimeEnvironment {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ProgramRuntimeEnvironment").field(&Arc::as_ptr(&self.0)).finish()
    }
}

impl Deref for ProgramRuntimeEnvironment {
    type Target = Arc<BuiltinProgram<InvokeContext<'static, 'static>>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Hash for ProgramRuntimeEnvironment {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::ptr::hash(Arc::as_ptr(&self.0), state);
    }
}

impl PartialEq for ProgramRuntimeEnvironment {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ProgramRuntimeEnvironment {}

pub const MAX_LOADED_ENTRY_COUNT: usize = 512;

/// Actual payload of [ProgramCacheEntry].
#[derive(Default)]
pub enum ProgramCacheEntryType {
    /// Program failed verification for the current runtime environment.
    FailedVerification(ProgramRuntimeEnvironment),
    /// Program is unavailable or intentionally closed.
    #[default]
    Closed,
    /// Retained for API compatibility with older delayed-visibility flows.
    DelayVisibility,
    /// Program was verified but is not currently compiled.
    Unloaded(ProgramRuntimeEnvironment),
    /// Verified and compiled program.
    Loaded(Executable<InvokeContext<'static, 'static>>),
    /// Builtin program shipped with the runtime.
    Builtin(BuiltinProgram<InvokeContext<'static, 'static>>),
}

impl Debug for ProgramCacheEntryType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProgramCacheEntryType::FailedVerification(_) => {
                write!(f, "ProgramCacheEntryType::FailedVerification")
            }
            ProgramCacheEntryType::Closed => write!(f, "ProgramCacheEntryType::Closed"),
            ProgramCacheEntryType::DelayVisibility => {
                write!(f, "ProgramCacheEntryType::DelayVisibility")
            }
            ProgramCacheEntryType::Unloaded(_) => write!(f, "ProgramCacheEntryType::Unloaded"),
            ProgramCacheEntryType::Loaded(_) => write!(f, "ProgramCacheEntryType::Loaded"),
            ProgramCacheEntryType::Builtin(_) => write!(f, "ProgramCacheEntryType::Builtin"),
        }
    }
}

impl ProgramCacheEntryType {
    /// Returns the runtime environment when this entry keeps one.
    pub fn get_environment(&self) -> Option<&ProgramRuntimeEnvironment> {
        match self {
            ProgramCacheEntryType::Loaded(program) => {
                // SAFETY: `ProgramRuntimeEnvironment` is transparent over the loader Arc.
                Some(unsafe { ProgramRuntimeEnvironment::from_ref(program.get_loader()) })
            }
            ProgramCacheEntryType::FailedVerification(env)
            | ProgramCacheEntryType::Unloaded(env) => Some(env),
            _ => None,
        }
    }
}

/// Single cache entry for a program address.
#[derive(Debug, Default)]
pub struct ProgramCacheEntry {
    pub program: ProgramCacheEntryType,
}

impl ProgramCacheEntry {
    /// Creates a new user program.
    pub fn new(
        program_runtime_environment: ProgramRuntimeEnvironment,
        elf_bytes: &[u8],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_internal(program_runtime_environment, elf_bytes, false)
    }

    /// Reloads a previously verified user program without re-running the verifier.
    ///
    /// # Safety
    ///
    /// Callers must ensure `elf_bytes` were already verified for the provided
    /// runtime environment.
    pub unsafe fn reload(
        program_runtime_environment: ProgramRuntimeEnvironment,
        elf_bytes: &[u8],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_internal(program_runtime_environment, elf_bytes, true)
    }

    fn new_internal(
        program_runtime_environment: ProgramRuntimeEnvironment,
        elf_bytes: &[u8],
        reloading: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Some architectures build without JIT support.
        #[allow(unused_mut)]
        let mut executable =
            Executable::load(elf_bytes, Arc::clone(&*program_runtime_environment))?;
        if !reloading {
            executable.verify::<RequisiteVerifier>()?;
        }

        #[cfg(all(not(target_os = "windows"), target_arch = "x86_64"))]
        executable.jit_compile()?;

        Ok(Self {
            program: ProgramCacheEntryType::Loaded(executable),
        })
    }

    /// Creates a new built-in program.
    pub fn new_builtin(builtin_function: BuiltinFunctionWithContext) -> Self {
        let mut program = BuiltinProgram::new_builtin();
        program.register_function("entrypoint", builtin_function).unwrap();
        Self {
            program: ProgramCacheEntryType::Builtin(program),
        }
    }
}

/// Shared runtime environments keyed by feature configuration.
#[derive(Clone, Debug)]
pub struct ProgramRuntimeEnvironments {
    execution: ProgramRuntimeEnvironment,
}

impl ProgramRuntimeEnvironments {
    pub fn new(execution: ProgramRuntimeEnvironment) -> Self {
        Self { execution }
    }

    pub fn get_env_for_execution(&self) -> &ProgramRuntimeEnvironment {
        &self.execution
    }
}

impl Default for ProgramRuntimeEnvironments {
    fn default() -> Self {
        let empty_loader =
            ProgramRuntimeEnvironment::from(BuiltinProgram::new_loader(Config::default()));
        Self::new(empty_loader.clone())
    }
}

/// Global program cache shared across transaction batches.
#[derive(Default)]
pub struct ProgramCache {
    index: scc::HashMap<Pubkey, Arc<ProgramCacheEntry>>,
}

impl Debug for ProgramCache {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgramCache").field("index_len", &self.index.len()).finish()
    }
}

/// Local view into [ProgramCache] used by a transaction batch.
#[derive(Clone, Debug, Default)]
pub struct ProgramCacheForTxBatch {
    entries: HashMap<Pubkey, Arc<ProgramCacheEntry>>,
    modified_entries: HashMap<Pubkey, Arc<ProgramCacheEntry>>,
    slot: Slot,
    pub hit_max_limit: bool,
    pub loaded_missing: bool,
    pub merged_modified: bool,
}

impl ProgramCacheForTxBatch {
    pub fn new(slot: Slot) -> Self {
        Self {
            entries: HashMap::new(),
            modified_entries: HashMap::new(),
            slot,
            hit_max_limit: false,
            loaded_missing: false,
            merged_modified: false,
        }
    }

    pub fn replenish(&mut self, key: Pubkey, entry: Arc<ProgramCacheEntry>) {
        self.entries.insert(key, entry);
    }

    pub fn store_modified_entry(&mut self, key: Pubkey, entry: Arc<ProgramCacheEntry>) {
        self.modified_entries.insert(key, entry);
    }

    pub fn drain_modified_entries(&mut self) -> HashMap<Pubkey, Arc<ProgramCacheEntry>> {
        std::mem::take(&mut self.modified_entries)
    }

    pub fn find(&self, key: &Pubkey) -> Option<Arc<ProgramCacheEntry>> {
        self.modified_entries.get(key).or_else(|| self.entries.get(key)).cloned()
    }

    pub fn slot(&self) -> Slot {
        self.slot
    }

    pub fn set_slot_for_tests(&mut self, slot: Slot) {
        self.slot = slot;
    }

    pub fn merge(&mut self, modified_entries: &HashMap<Pubkey, Arc<ProgramCacheEntry>>) {
        for (key, entry) in modified_entries {
            self.merged_modified = true;
            self.replenish(*key, entry.clone());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl ProgramCache {
    pub fn assign_program(&self, key: Pubkey, entry: Arc<ProgramCacheEntry>) {
        self.index.upsert_sync(key, entry);
    }

    pub fn get(&self, key: &Pubkey) -> Option<Arc<ProgramCacheEntry>> {
        self.index.read_sync(key, |_, entry| entry.clone())
    }

    pub fn merge(&self, modified_entries: &HashMap<Pubkey, Arc<ProgramCacheEntry>>) {
        for (key, entry) in modified_entries {
            self.assign_program(*key, entry.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{
            ProgramCache, ProgramCacheEntry, ProgramCacheEntryType, ProgramRuntimeEnvironment,
        },
        solana_pubkey::Pubkey,
        solana_sbpf::{elf::Executable, program::BuiltinProgram},
        solana_svm_type_overrides::sync::Arc,
    };

    static MOCK_ENVIRONMENT: std::sync::OnceLock<ProgramRuntimeEnvironment> =
        std::sync::OnceLock::new();

    fn mock_env() -> ProgramRuntimeEnvironment {
        MOCK_ENVIRONMENT
            .get_or_init(|| ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock()))
            .clone()
    }

    fn test_entry() -> Arc<ProgramCacheEntry> {
        let elf = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/noop_aligned.so"
        ))
        .unwrap();
        let environment = mock_env();
        let executable = Executable::load(&elf, Arc::clone(&*environment)).unwrap();
        Arc::new(ProgramCacheEntry {
            program: ProgramCacheEntryType::Loaded(executable),
        })
    }

    #[test]
    fn assign_program_makes_entry_retrievable() {
        let cache = ProgramCache::default();
        let key = Pubkey::new_unique();
        let entry = test_entry();
        cache.assign_program(key, entry.clone());

        let fetched = cache.get(&key);
        assert!(fetched.is_some());
        assert!(Arc::ptr_eq(&fetched.unwrap(), &entry));
    }
}
