use std::error::Error;

use agave_syscalls::SyscallMemcpy;
use solana_account::AccountMode;
use solana_program_runtime::{invoke_context::InvokeContext, memory::translate_slice};
use solana_sbpf::{
    error::EbpfError, memory_region::AccessType, program::BuiltinFunctionDefinition,
};

use crate::metrics;

/// Allows legacy Anchor exit serialization after undelegation when bytes match.
/// The fallback never writes or changes mapping permissions; direct SBF stores
/// into the frozen account remain prohibited.
pub(crate) struct Memcpy;

impl BuiltinFunctionDefinition<InvokeContext<'_, '_>> for Memcpy {
    type Error = Box<dyn Error>;

    fn rust(
        context: &mut InvokeContext<'_, '_>,
        dst: u64,
        src: u64,
        len: u64,
        arg4: u64,
        arg5: u64,
    ) -> Result<u64, Self::Error> {
        // Upstream owns charging, overlap checks, and normal copies. Do not retry
        // the syscall: the rejected attempt has already consumed compute units.
        SyscallMemcpy::rust(context, dst, src, len, arg4, arg5).or_else(
            |error| match unchanged_copy(context, error.as_ref(), dst, src, len) {
                Some(()) => {
                    metrics::memcpy_fallback();
                    Ok(0)
                }
                None => Err(error),
            },
        )
    }
}

/// Proves the rejected copy is a no-op on instruction-writable, undelegating data.
/// Any failed check returns `None`, preserving the original upstream error rather
/// than exposing a failure from the fallback's read-only validation.
fn unchanged_copy(
    context: &InvokeContext<'_, '_>,
    error: &(dyn Error + 'static),
    dst: u64,
    src: u64,
    len: u64,
) -> Option<()> {
    let EbpfError::AccessViolation(AccessType::Store, address, size, _) =
        error.downcast_ref::<EbpfError>()?
    else {
        return None;
    };
    if *address != dst || *size != len {
        return None;
    }
    let mapping = context.memory_contexts.memory_mapping().ok()?;
    let (_, region) = mapping.find_region(dst)?;
    if region.writable {
        return None;
    }
    let metadata = &context.get_syscall_context().ok()?.accounts_metadata;
    let index = metadata.iter().position(|account| account.vm_data_addr == region.vm_addr)?;
    let transaction = &context.transaction_context;
    let instruction = transaction.get_current_instruction_context().ok()?;
    let account = instruction.instruction_accounts().get(index)?;
    if !account.is_writable() {
        return None;
    }
    let account = transaction.accounts().try_borrow(account.index_in_transaction).ok()?;
    if !account.is(AccountMode::Undelegating) {
        return None;
    }
    // Read translations exclude reserved growth space and reject invalid ranges.
    let aligned = context.get_check_aligned();
    let destination = translate_slice::<u8>(mapping, dst, len, aligned).ok()?;
    let source = translate_slice::<u8>(mapping, src, len, aligned).ok()?;
    (destination == source).then_some(())
}
