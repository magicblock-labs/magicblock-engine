use {
    crate::{
        IndexOfAccount, MAX_ACCOUNT_DATA_GROWTH_PER_INSTRUCTION, transaction::TransactionContext,
        transaction_accounts::AccountRefMut,
    },
    solana_account::{CoWAccount, ReadableAccount, WritableAccount},
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
};

/// Contains account meta data which varies between instruction.
///
/// It also contains indices to other structures for faster lookup.
///
/// This data structure is supposed to be shared with programs in ABIv2, so do not modify it
/// without consulting SIMD-0177.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InstructionAccount {
    /// Points to the account and its key in the `TransactionContext`
    pub index_in_transaction: IndexOfAccount,
    /// Is this account supposed to sign
    is_signer: u8,
    /// Is this account allowed to become writable
    is_writable: u8,
}

impl InstructionAccount {
    pub fn new(
        index_in_transaction: IndexOfAccount,
        is_signer: bool,
        is_writable: bool,
    ) -> InstructionAccount {
        InstructionAccount {
            index_in_transaction,
            is_signer: is_signer as u8,
            is_writable: is_writable as u8,
        }
    }

    pub fn is_signer(&self) -> bool {
        self.is_signer != 0
    }

    pub fn is_writable(&self) -> bool {
        self.is_writable != 0
    }

    pub fn set_is_signer(&mut self, value: bool) {
        self.is_signer = value as u8;
    }

    pub fn set_is_writable(&mut self, value: bool) {
        self.is_writable = value as u8;
    }
}

/// Account borrowed for the current instruction, with program mutation checks.
///
/// Changes require instruction writability, the relevant ownership permissions,
/// and a currently mutable account mode. Lifecycle dirty markers allow later
/// writeback, not additional program writes. Scalar setters preserve no-op values
/// after their Solana checks pass, without touching the account or its accounting.
#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
#[derive(Debug)]
pub struct BorrowedInstructionAccount<'a, 'ix_data> {
    pub(crate) transaction_context: &'a TransactionContext<'ix_data>,
    pub(crate) account: AccountRefMut<'a>,
    pub(crate) instruction_account: InstructionAccount,
    pub(crate) index_in_transaction_of_instruction_program: IndexOfAccount,
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl BorrowedInstructionAccount<'_, '_> {
    /// Returns the index of this account (transaction wide)
    #[inline]
    pub fn get_index_in_transaction(&self) -> IndexOfAccount {
        self.instruction_account.index_in_transaction
    }

    /// Returns the public key of this account (transaction wide)
    #[inline]
    pub fn get_key(&self) -> &Pubkey {
        self.transaction_context
            .get_key_of_account_at_index(self.instruction_account.index_in_transaction)
            .unwrap()
    }

    /// Returns the owner of this account (transaction wide)
    #[inline]
    pub fn get_owner(&self) -> &Pubkey {
        self.account.owner()
    }

    /// Assigns a new owner to a writable, mutable account owned by this program.
    ///
    /// Data must be empty or zeroed. An unchanged owner skips the mode check only
    /// after ownership, instruction writability, and data checks have passed.
    pub fn set_owner(&mut self, pubkey: &[u8]) -> Result<(), InstructionError> {
        if !self.is_owned_by_current_program() || !self.is_writable() {
            return Err(InstructionError::ModifiedProgramId);
        }
        if !is_zeroed(self.get_data()) {
            return Err(InstructionError::ModifiedProgramId);
        }
        // don't touch the account if the owner does not change
        if self.get_owner().to_bytes() == pubkey {
            return Ok(());
        }
        self.check_mutable()?;
        self.touch()?;
        self.account.copy_into_owner_from_slice(pubkey);
        Ok(())
    }

    /// Returns the number of lamports of this account (transaction wide)
    #[inline]
    pub fn get_lamports(&self) -> u64 {
        self.account.lamports()
    }

    /// Replaces the balance and records its delta for instruction balance checks.
    ///
    /// Debits require ownership; any change requires instruction writability and
    /// a mutable mode. An unchanged balance skips the mode check and accounting,
    /// so CPI synchronization can preserve a legally locked account.
    pub fn set_lamports(&mut self, lamports: u64) -> Result<(), InstructionError> {
        let old_lamports = self.get_lamports();
        // An account not owned by the program cannot have its balance decrease
        if !self.is_owned_by_current_program() && lamports < old_lamports {
            return Err(InstructionError::ExternalAccountLamportSpend);
        }
        // The balance of read-only may not change
        if !self.is_writable() {
            return Err(InstructionError::ReadonlyLamportChange);
        }
        // don't touch the account if the lamports do not change
        if old_lamports == lamports {
            return Ok(());
        }
        self.check_mutable()?;

        // The difference between two u64 balances always fits in i128.
        let delta = i128::from(lamports) - i128::from(old_lamports);
        self.transaction_context.accounts.add_lamports_delta(delta)?;

        self.touch()?;
        self.account.set_lamports(lamports);
        Ok(())
    }

    /// Adds lamports to this account (transaction wide)
    pub fn checked_add_lamports(&mut self, lamports: u64) -> Result<(), InstructionError> {
        self.set_lamports(
            self.get_lamports()
                .checked_add(lamports)
                .ok_or(InstructionError::ArithmeticOverflow)?,
        )
    }

    /// Subtracts lamports from this account (transaction wide)
    pub fn checked_sub_lamports(&mut self, lamports: u64) -> Result<(), InstructionError> {
        self.set_lamports(
            self.get_lamports()
                .checked_sub(lamports)
                .ok_or(InstructionError::ArithmeticOverflow)?,
        )
    }

    /// Returns a read-only slice of the account data (transaction wide)
    #[inline]
    pub fn get_data(&self) -> &[u8] {
        self.account.data()
    }

    /// Returns a writable slice of the account data (transaction wide)
    pub fn get_data_mut(&mut self) -> Result<&mut [u8], InstructionError> {
        self.can_data_be_changed()?;
        self.touch()?;
        self.make_data_mut();
        Ok(self.account.data_as_mut_slice())
    }

    /// Overwrites the account data and size (transaction wide).
    ///
    /// Call this when you have a slice of data you do not own and want to
    /// replace the account data with it.
    pub fn set_data_from_slice(&mut self, data: &[u8]) -> Result<(), InstructionError> {
        self.can_data_be_resized(data.len())?;
        self.touch()?;
        self.update_accounts_resize_delta(data.len())?;
        // Note that we intentionally don't call self.make_data_mut() here.  make_data_mut() will
        // allocate + memcpy the current data if self.account is shared. We don't need the memcpy
        // here tho because account.set_data_from_slice(data) is going to replace the content
        // anyway.
        self.account.set_data_from_slice(data);

        Ok(())
    }

    /// Resizes the account data (transaction wide)
    ///
    /// Fills it with zeros at the end if is extended or truncates at the end otherwise.
    pub fn set_data_length(&mut self, new_length: usize) -> Result<(), InstructionError> {
        self.can_data_be_resized(new_length)?;
        // don't touch the account if the length does not change
        if self.get_data().len() == new_length {
            return Ok(());
        }
        self.touch()?;
        self.update_accounts_resize_delta(new_length)?;
        self.account.resize(new_length, 0);
        Ok(())
    }

    /// Appends all elements in a slice to the account
    pub fn extend_from_slice(&mut self, data: &[u8]) -> Result<(), InstructionError> {
        let new_len = self.get_data().len().saturating_add(data.len());
        self.can_data_be_resized(new_len)?;

        if data.is_empty() {
            return Ok(());
        }

        self.touch()?;
        self.update_accounts_resize_delta(new_len)?;
        // Even if extend_from_slice never reduces capacity, still realloc using
        // make_data_mut() if necessary so that we grow the account of the full
        // max realloc length in one go, avoiding smaller reallocations.
        self.make_data_mut();
        self.account.extend_from_slice(data);
        Ok(())
    }

    /// Returns whether account data must be mapped through the CoW handler.
    ///
    /// Owned shared buffers and borrowed account images both need first-write
    /// translation before the VM can mutate them.
    pub fn is_shared(&self) -> bool {
        self.account.is_shared() || matches!(self.account.cow(), CoWAccount::Borrowed(_))
    }

    fn make_data_mut(&mut self) {
        // Reserve the maximum per-instruction growth before mutating shared buffers or borrowed
        // account images. Borrowed images with enough spare capacity can remain borrowed.
        if self.is_shared() {
            self.account.reserve(MAX_ACCOUNT_DATA_GROWTH_PER_INSTRUCTION);
        }
    }

    /// Deserializes the account data into a state
    #[cfg(feature = "bincode")]
    pub fn get_state<T: serde::de::DeserializeOwned>(&self) -> Result<T, InstructionError> {
        bincode::deserialize(self.account.data()).map_err(|_| InstructionError::InvalidAccountData)
    }

    /// Serializes a state into the account data
    #[cfg(feature = "bincode")]
    pub fn set_state<T: serde::Serialize>(&mut self, state: &T) -> Result<(), InstructionError> {
        let data = self.get_data_mut()?;
        let serialized_size =
            bincode::serialized_size(state).map_err(|_| InstructionError::GenericError)?;
        if serialized_size > data.len() as u64 {
            return Err(InstructionError::AccountDataTooSmall);
        }
        bincode::serialize_into(&mut *data, state).map_err(|_| InstructionError::GenericError)?;
        Ok(())
    }

    // Returns whether or the lamports currently in the account is sufficient for rent exemption should the
    // data be resized to the given size
    pub fn is_rent_exempt_at_data_length(&self, data_length: usize) -> bool {
        self.transaction_context.rent.is_exempt(self.get_lamports(), data_length)
    }

    /// Returns whether this account is executable (transaction wide)
    #[inline]
    #[deprecated(since = "2.1.0", note = "Use `get_owner` instead")]
    pub fn is_executable(&self) -> bool {
        #[allow(deprecated)]
        self.account.executable()
    }

    /// Changes the executable flag on a rent-exempt account owned by this program.
    ///
    /// Instruction writability is required even for an unchanged value; mode
    /// permission and dirty accounting apply only when the flag changes.
    pub fn set_executable(&mut self, is_executable: bool) -> Result<(), InstructionError> {
        // To become executable an account must be rent exempt
        if !self
            .transaction_context
            .rent
            .is_exempt(self.get_lamports(), self.get_data().len())
        {
            return Err(InstructionError::ExecutableAccountNotRentExempt);
        }
        if !self.is_owned_by_current_program() || !self.is_writable() {
            return Err(InstructionError::ExecutableModified);
        }
        // don't touch the account if the executable flag does not change
        #[allow(deprecated)]
        if self.is_executable() == is_executable {
            return Ok(());
        }
        self.check_mutable()?;
        self.touch()?;
        self.account.set_executable(is_executable);
        Ok(())
    }

    /// Returns the rent epoch of this account (transaction wide)
    #[inline]
    pub fn get_rent_epoch(&self) -> u64 {
        self.account.rent_epoch()
    }

    /// Returns whether this account is a signer (instruction wide)
    pub fn is_signer(&self) -> bool {
        self.instruction_account.is_signer()
    }

    /// Returns the instruction's writable flag, independently of mode and owner.
    pub fn is_writable(&self) -> bool {
        self.instruction_account.is_writable()
    }

    /// Returns true if the owner of this account is the current `InstructionContext`s last program (instruction wide)
    pub fn is_owned_by_current_program(&self) -> bool {
        self.transaction_context
            .get_key_of_account_at_index(self.index_in_transaction_of_instruction_program)
            .map(|program_key| program_key == self.get_owner())
            .unwrap_or_default()
    }

    /// Checks instruction writability, ownership, and mode before data mutation.
    ///
    /// Setters and VM mappings share this check, including when rebuilding the
    /// caller's data regions after CPI changes an account's owner or mode.
    pub fn can_data_be_changed(&self) -> Result<(), InstructionError> {
        // and only if the account is writable
        if !self.is_writable() {
            return Err(InstructionError::ReadonlyDataModified);
        }
        // and only if we are the owner
        if !self.is_owned_by_current_program() {
            return Err(InstructionError::ExternalAccountDataModified);
        }
        self.check_mutable()
    }

    /// Requires a currently mutable mode, regardless of transaction dirty markers.
    fn check_mutable(&self) -> Result<(), InstructionError> {
        if self.account.mode().mutable() {
            Ok(())
        } else {
            Err(InstructionError::Immutable)
        }
    }

    /// Returns an error if the account data can not be resized to the given length
    pub fn can_data_be_resized(&self, new_len: usize) -> Result<(), InstructionError> {
        let old_len = self.get_data().len();
        if new_len != old_len {
            use solana_account::AccountMode;
            if !self.is_owned_by_current_program() {
                // Only the owner can change the length of the data
                return Err(InstructionError::AccountDataSizeChanged);
            } else if self.account.is(AccountMode::Ephemeral) {
                // Ephemeral accounts can only be resized with special builtin instruction
                return Err(InstructionError::InvalidRealloc);
            }
        }
        self.transaction_context.accounts.can_data_be_resized(old_len, new_len)?;
        self.can_data_be_changed()
    }

    fn touch(&self) -> Result<(), InstructionError> {
        self.transaction_context
            .accounts
            .touch(self.instruction_account.index_in_transaction)
    }

    fn update_accounts_resize_delta(&mut self, new_len: usize) -> Result<(), InstructionError> {
        self.transaction_context
            .accounts
            .update_accounts_resize_delta(self.get_data().len(), new_len)
    }
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
fn is_zeroed(buf: &[u8]) -> bool {
    const ZEROS_LEN: usize = 1024;
    const ZEROS: [u8; ZEROS_LEN] = [0; ZEROS_LEN];
    let mut chunks = buf.chunks_exact(ZEROS_LEN);

    #[allow(clippy::indexing_slicing)]
    {
        chunks.all(|chunk| chunk == &ZEROS[..])
            && chunks.remainder() == &ZEROS[..chunks.remainder().len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_account::{AccountBuilder, AccountMode, AccountSharedData, DirtyMarkers};
    use solana_rent::Rent;

    /// Proves scalar and data mutations reject immutable modes without changing
    /// state or lamport deltas, while no-ops preserve legal lifecycle markers.
    #[test]
    fn immutable_modes_reject_mutations() {
        use AccountMode::*;
        for mode in [Placeholder, ReadOnly, System, Transient, Closed] {
            let owner = Pubkey::new_unique();
            let initial = match mode {
                Transient => Delegated,
                Closed => Ephemeral,
                _ => mode,
            };
            let mut account = AccountBuilder::default()
                .mode(initial)
                .owner(owner)
                .lamports(Rent::default().minimum_balance(0))
                .build::<AccountSharedData>();
            if initial != mode {
                account.set_lifecycle(mode, account.slot()).unwrap();
                assert!(account.markers().contains(DirtyMarkers::MODE));
                assert!(account.mutable(), "the transition still permits writeback");
            }
            let before = account.clone();
            let mut context = TransactionContext::new(
                vec![(owner, AccountSharedData::default()), (Pubkey::new_unique(), account)],
                Rent::default(),
                1,
                1,
                1,
            );
            context
                .configure_top_level_instruction_for_tests(
                    0,
                    vec![InstructionAccount::new(1, false, true)],
                    vec![],
                )
                .unwrap();
            context.push().unwrap();
            let instruction = context.get_current_instruction_context().unwrap();
            let mut account = instruction.try_borrow_instruction_account(0).unwrap();
            let balance = account.get_lamports();
            assert_eq!(account.set_lamports(balance), Ok(()));
            assert_eq!(account.set_owner(owner.as_ref()), Ok(()));
            assert_eq!(account.set_executable(false), Ok(()));
            assert_eq!(
                account.set_lamports(balance + 1),
                Err(InstructionError::Immutable)
            );
            assert_eq!(
                account.set_lamports(balance - 1),
                Err(InstructionError::Immutable)
            );
            assert_eq!(
                account.set_owner(Pubkey::new_unique().as_ref()),
                Err(InstructionError::Immutable)
            );
            assert_eq!(
                account.set_executable(true),
                Err(InstructionError::Immutable)
            );
            assert_eq!(account.get_data_mut(), Err(InstructionError::Immutable));
            assert_eq!(
                account.set_data_from_slice(&[1]),
                Err(InstructionError::Immutable)
            );
            assert_eq!(account.set_data_length(1), Err(InstructionError::Immutable));
            assert_eq!(
                account.extend_from_slice(&[1]),
                Err(InstructionError::Immutable)
            );
            assert_eq!(**account.account, before, "{mode:?}");
            assert_eq!(account.account.markers(), before.markers());
            assert_eq!(context.accounts.get_lamports_delta(), 0);
            drop(account);
            context.pop().unwrap();
        }
    }
}
