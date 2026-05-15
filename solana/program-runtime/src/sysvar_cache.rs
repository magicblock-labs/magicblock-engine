use solana_epoch_rewards::EpochRewards;
#[allow(deprecated)]
use solana_sysvar::fees::Fees;
#[allow(deprecated)]
use solana_sysvar::recent_blockhashes::RecentBlockhashes;
use {
    crate::invoke_context::InvokeContext,
    serde::de::DeserializeOwned,
    solana_clock::Clock,
    solana_epoch_schedule::EpochSchedule,
    solana_instruction::error::InstructionError,
    solana_last_restart_slot::LastRestartSlot,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_slot_hashes::SlotHashes,
    solana_svm_type_overrides::sync::Arc,
    solana_sysvar_id::SysvarId,
    solana_transaction_context::{IndexOfAccount, instruction::InstructionContext},
};

/// Serialized sysvars exposed to programs during execution.
#[derive(Default, Debug)]
pub struct SysvarCache {
    // Full account data, including any trailing zero bytes.
    clock: Option<Vec<u8>>,
    epoch_schedule: Option<Vec<u8>>,
    rent: Option<Vec<u8>>,
    slot_hashes: Option<Vec<u8>>,
    last_restart_slot: Option<Vec<u8>>,

    // Object representations of large sysvars used by native builtins.
    slot_hashes_obj: Option<SlotHashes>,

    #[allow(deprecated)]
    recent_blockhashes: Option<RecentBlockhashes>,
}

impl SysvarCache {
    /// Returns the serialized sysvar buffer for `SyscallGetSysvar`.
    pub fn sysvar_id_to_buffer(&self, sysvar_id: &Pubkey) -> &Option<Vec<u8>> {
        if Clock::check_id(sysvar_id) {
            &self.clock
        } else if EpochSchedule::check_id(sysvar_id) {
            &self.epoch_schedule
        } else if Rent::check_id(sysvar_id) {
            &self.rent
        } else if SlotHashes::check_id(sysvar_id) {
            &self.slot_hashes
        } else if LastRestartSlot::check_id(sysvar_id) {
            &self.last_restart_slot
        } else {
            &None
        }
    }

    fn get_sysvar_obj<T: DeserializeOwned>(
        &self,
        sysvar_id: &Pubkey,
    ) -> Result<Arc<T>, InstructionError> {
        if let Some(sysvar_buf) = self.sysvar_id_to_buffer(sysvar_id) {
            bincode::deserialize(sysvar_buf)
                .map(Arc::new)
                .map_err(|_| InstructionError::UnsupportedSysvar)
        } else {
            Err(InstructionError::UnsupportedSysvar)
        }
    }

    /// Stores a serialized clock sysvar.
    pub fn set_clock(&mut self, clock: &Clock) {
        let buffer = self.clock.get_or_insert_default();
        buffer.clear();
        // bincode doesn't fail when writing to a correctly sized sysvar buffer.
        let _ = bincode::serialize_into(buffer, clock);
    }

    /// Returns the cached clock sysvar.
    pub fn get_clock(&self) -> Result<Arc<Clock>, InstructionError> {
        self.get_sysvar_obj(&Clock::id())
    }

    /// Returns the cached rent sysvar.
    pub fn get_rent(&self) -> Result<Arc<Rent>, InstructionError> {
        self.get_sysvar_obj(&Rent::id())
    }

    /// Returns the cached last-restart-slot sysvar.
    pub fn get_last_restart_slot(&self) -> Result<Arc<LastRestartSlot>, InstructionError> {
        self.get_sysvar_obj(&LastRestartSlot::id())
    }

    /// Returns the cached slot hashes sysvar.
    pub fn get_slot_hashes(&self) -> Result<Arc<SlotHashes>, InstructionError> {
        self.slot_hashes_obj
            .as_ref()
            .map(|s| Arc::new(SlotHashes::new(s.slot_hashes())))
            .ok_or(InstructionError::UnsupportedSysvar)
    }

    #[deprecated]
    #[allow(deprecated)]
    /// Returns the cached recent-blockhashes sysvar.
    pub fn get_recent_blockhashes(&self) -> Result<Arc<RecentBlockhashes>, InstructionError> {
        self.recent_blockhashes
            .clone()
            .ok_or(InstructionError::UnsupportedSysvar)
            .map(Arc::new)
    }

    /// Returns the (deprecated) fees sysvar; this engine always reports defaults.
    #[allow(deprecated)]
    pub fn get_fees(&self) -> Result<Arc<Fees>, InstructionError> {
        Ok(Arc::new(Default::default()))
    }

    /// Returns the epoch-schedule sysvar; this engine always reports defaults.
    pub fn get_epoch_schedule(&self) -> Result<Arc<EpochSchedule>, InstructionError> {
        Ok(Arc::new(Default::default()))
    }

    /// Returns the epoch-rewards sysvar; this engine always reports defaults.
    pub fn get_epoch_rewards(&self) -> Result<Arc<EpochRewards>, InstructionError> {
        Ok(Arc::new(Default::default()))
    }

    /// Fills missing sysvars by asking the caller for serialized account data.
    pub fn fill_missing_entries<F: FnMut(&Pubkey, &mut dyn FnMut(&[u8]))>(
        &mut self,
        mut get_account_data: F,
    ) {
        if self.clock.is_none() {
            get_account_data(&Clock::id(), &mut |data: &[u8]| {
                if bincode::deserialize::<Clock>(data).is_ok() {
                    self.clock = Some(data.to_vec());
                }
            });
        }

        if self.epoch_schedule.is_none() {
            get_account_data(&EpochSchedule::id(), &mut |data: &[u8]| {
                if bincode::deserialize::<EpochSchedule>(data).is_ok() {
                    self.epoch_schedule = Some(data.to_vec());
                }
            });
        }

        if self.rent.is_none() {
            get_account_data(&Rent::id(), &mut |data: &[u8]| {
                if bincode::deserialize::<Rent>(data).is_ok() {
                    self.rent = Some(data.to_vec());
                }
            });
        }

        if self.slot_hashes.is_none() {
            get_account_data(&SlotHashes::id(), &mut |data: &[u8]| {
                if let Ok(obj) = bincode::deserialize::<SlotHashes>(data) {
                    self.slot_hashes = Some(data.to_vec());
                    self.slot_hashes_obj = Some(obj);
                }
            });
        }

        if self.last_restart_slot.is_none() {
            get_account_data(&LastRestartSlot::id(), &mut |data: &[u8]| {
                if bincode::deserialize::<LastRestartSlot>(data).is_ok() {
                    self.last_restart_slot = Some(data.to_vec());
                }
            });
        }

        #[allow(deprecated)]
        if self.recent_blockhashes.is_none() {
            get_account_data(&RecentBlockhashes::id(), &mut |data: &[u8]| {
                if let Ok(recent_blockhashes) = bincode::deserialize(data) {
                    self.recent_blockhashes = Some(recent_blockhashes);
                }
            });
        }
    }

    /// Clears all cached sysvars.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Sysvar accessors that also verify the instruction account matches the
/// requested sysvar id.
pub mod get_sysvar_with_account_check {
    use super::*;

    fn check_sysvar_account<S: SysvarId>(
        instruction_context: &InstructionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<(), InstructionError> {
        if !S::check_id(
            instruction_context.get_key_of_instruction_account(instruction_account_index)?,
        ) {
            return Err(InstructionError::InvalidArgument);
        }
        Ok(())
    }

    /// Returns the clock sysvar after checking the provided instruction account.
    pub fn clock(
        invoke_context: &InvokeContext,
        instruction_context: &InstructionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<Arc<Clock>, InstructionError> {
        check_sysvar_account::<Clock>(instruction_context, instruction_account_index)?;
        invoke_context.get_sysvar_cache().get_clock()
    }

    /// Returns the rent sysvar after checking the provided instruction account.
    pub fn rent(
        invoke_context: &InvokeContext,
        instruction_context: &InstructionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<Arc<Rent>, InstructionError> {
        check_sysvar_account::<Rent>(instruction_context, instruction_account_index)?;
        invoke_context.get_sysvar_cache().get_rent()
    }

    /// Returns slot hashes after checking the provided instruction account.
    pub fn slot_hashes(
        invoke_context: &InvokeContext,
        instruction_context: &InstructionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<Arc<SlotHashes>, InstructionError> {
        check_sysvar_account::<SlotHashes>(instruction_context, instruction_account_index)?;
        invoke_context.get_sysvar_cache().get_slot_hashes()
    }

    #[allow(deprecated)]
    /// Returns recent blockhashes after checking the provided instruction account.
    pub fn recent_blockhashes(
        invoke_context: &InvokeContext,
        instruction_context: &InstructionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<Arc<RecentBlockhashes>, InstructionError> {
        check_sysvar_account::<RecentBlockhashes>(instruction_context, instruction_account_index)?;
        invoke_context.get_sysvar_cache().get_recent_blockhashes()
    }

    pub fn last_restart_slot(
        invoke_context: &InvokeContext,
        instruction_context: &InstructionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<Arc<LastRestartSlot>, InstructionError> {
        check_sysvar_account::<LastRestartSlot>(instruction_context, instruction_account_index)?;
        invoke_context.get_sysvar_cache().get_last_restart_slot()
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_sysvar::SysvarSerialize, test_case::test_case};

    // sysvar cache provides the full account data of a sysvar
    // the setters MUST NOT be changed to serialize an object representation
    // it is required that the syscall be able to access the full buffer as it exists onchain
    // this is meant to cover the cases:
    // * account data is larger than struct sysvar
    // * vector sysvar has fewer than its maximum entries
    // if at any point the data is roundtripped through bincode, the vector will shrink
    #[test_case(Clock::default(); "clock")]
    #[test_case(Rent::default(); "rent")]
    #[test_case(SlotHashes::default(); "slot_hashes")]
    #[test_case(LastRestartSlot::default(); "last_restart_slot")]
    fn test_sysvar_cache_preserves_bytes<T: SysvarSerialize>(_: T) {
        let id = T::id();
        let size = T::size_of().saturating_mul(2);
        let in_buf = vec![0; size];

        let mut sysvar_cache = SysvarCache::default();
        sysvar_cache.fill_missing_entries(|pubkey, callback| {
            if *pubkey == id {
                callback(&in_buf)
            }
        });
        let sysvar_cache = sysvar_cache;

        let out_buf = sysvar_cache.sysvar_id_to_buffer(&id).clone().unwrap();

        assert_eq!(out_buf, in_buf);
    }
}
