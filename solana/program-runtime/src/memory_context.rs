use {
    crate::invoke_context::BpfAllocator, solana_instruction::error::InstructionError,
    solana_sbpf::memory_region::MemoryMapping,
};

enum MemoryContextType {
    ABIv1(MemoryContext),
    Placeholder,
}

pub struct MemoryContexts {
    contexts: Vec<MemoryContextType>,
}

impl MemoryContexts {
    pub(crate) fn new() -> Self {
        Self { contexts: Vec::new() }
    }

    pub fn set_memory_context_abi_v1(
        &mut self,
        memory_context: MemoryContext,
    ) -> Result<(), InstructionError> {
        *self.contexts.last_mut().ok_or(InstructionError::CallDepth)? =
            MemoryContextType::ABIv1(memory_context);
        Ok(())
    }

    pub fn memory_context_mut_abi_v1(&mut self) -> Result<&mut MemoryContext, InstructionError> {
        match self.contexts.last_mut().ok_or(InstructionError::CallDepth)? {
            MemoryContextType::ABIv1(context) => Ok(context),
            MemoryContextType::Placeholder => Err(InstructionError::ProgramEnvironmentSetupFailure),
        }
    }

    pub fn memory_mapping(&self) -> Result<&MemoryMapping, InstructionError> {
        match self.contexts.last().ok_or(InstructionError::CallDepth)? {
            MemoryContextType::ABIv1(context) => Ok(&context.memory_mapping),
            MemoryContextType::Placeholder => Err(InstructionError::ProgramEnvironmentSetupFailure),
        }
    }

    pub fn memory_mapping_mut(&mut self) -> Result<&mut MemoryMapping, InstructionError> {
        match self.contexts.last_mut().ok_or(InstructionError::CallDepth)? {
            MemoryContextType::ABIv1(context) => Ok(&mut context.memory_mapping),
            MemoryContextType::Placeholder => Err(InstructionError::ProgramEnvironmentSetupFailure),
        }
    }

    pub fn push_placeholder(&mut self) {
        self.contexts.push(MemoryContextType::Placeholder);
    }

    pub fn pop(&mut self) {
        self.contexts.pop();
    }
}

pub struct MemoryContext {
    pub allocator: BpfAllocator,
    pub accounts_metadata: Vec<SerializedAccountMetadata>,
    memory_mapping: Box<MemoryMapping>,
}

impl MemoryContext {
    pub fn new(
        allocator: BpfAllocator,
        accounts_metadata: Vec<SerializedAccountMetadata>,
        memory_mapping: MemoryMapping,
    ) -> Self {
        Self {
            allocator,
            accounts_metadata,
            memory_mapping: Box::new(memory_mapping),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SerializedAccountMetadata {
    pub vm_addr: u64,
    pub original_data_len: usize,
    pub vm_data_addr: u64,
    pub vm_key_addr: u64,
    pub vm_lamports_addr: u64,
    pub vm_owner_addr: u64,
}
