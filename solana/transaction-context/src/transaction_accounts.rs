use {
    crate::{IndexOfAccount, MAX_ACCOUNT_DATA_GROWTH_PER_TRANSACTION, MAX_ACCOUNT_DATA_LEN},
    solana_account::AccountSharedData,
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    std::{
        cell::{Cell, UnsafeCell},
        ops::{Deref, DerefMut},
    },
};

#[derive(Debug, PartialEq)]
#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
pub struct TransactionAccountView<'a> {
    account: &'a AccountSharedData,
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl Deref for TransactionAccountView<'_> {
    type Target = AccountSharedData;
    fn deref(&self) -> &Self::Target {
        self.account
    }
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl PartialEq<AccountSharedData> for TransactionAccountView<'_> {
    fn eq(&self, other: &AccountSharedData) -> bool {
        self.account == other
    }
}

#[derive(Debug)]
#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
pub struct TransactionAccountViewMut<'a> {
    account: &'a mut AccountSharedData,
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl TransactionAccountViewMut<'_> {
    pub(crate) fn reserve(&mut self, additional: usize) {
        self.account.cow_mut().reserve(additional);
    }
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl Deref for TransactionAccountViewMut<'_> {
    type Target = AccountSharedData;
    fn deref(&self) -> &Self::Target {
        self.account
    }
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl DerefMut for TransactionAccountViewMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.account
    }
}

//
/// An account key and the matching account
#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
pub type KeyedAccountSharedData = (Pubkey, AccountSharedData);
#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
pub(crate) type DeconstructedTransactionAccounts =
    (Vec<KeyedAccountSharedData>, Box<[Cell<bool>]>, Cell<i64>);

#[derive(Debug)]
#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
pub struct TransactionAccounts {
    accounts: Box<[UnsafeCell<KeyedAccountSharedData>]>,
    borrow_counters: Box<[BorrowCounter]>,
    touched_flags: Box<[Cell<bool>]>,
    resize_delta: Cell<i64>,
    lamports_delta: Cell<i128>,
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl TransactionAccounts {
    pub(crate) fn new(accounts: Vec<KeyedAccountSharedData>) -> TransactionAccounts {
        let touched_flags = vec![Cell::new(false); accounts.len()].into_boxed_slice();
        let borrow_counters = vec![BorrowCounter::default(); accounts.len()].into_boxed_slice();
        let accounts =
            accounts.into_iter().map(UnsafeCell::new).collect::<Vec<_>>().into_boxed_slice();

        TransactionAccounts {
            accounts,
            borrow_counters,
            touched_flags,
            resize_delta: Cell::new(0),
            lamports_delta: Cell::new(0),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn touch(&self, index: IndexOfAccount) -> Result<(), InstructionError> {
        self.touched_flags
            .get(index as usize)
            .ok_or(InstructionError::MissingAccount)?
            .set(true);
        Ok(())
    }

    pub(crate) fn update_accounts_resize_delta(
        &self,
        old_len: usize,
        new_len: usize,
    ) -> Result<(), InstructionError> {
        let accounts_resize_delta = self.resize_delta.get();
        self.resize_delta.set(
            accounts_resize_delta.saturating_add((new_len as i64).saturating_sub(old_len as i64)),
        );
        Ok(())
    }

    pub(crate) fn can_data_be_resized(
        &self,
        old_len: usize,
        new_len: usize,
    ) -> Result<(), InstructionError> {
        // The new length can not exceed the maximum permitted length
        if new_len > MAX_ACCOUNT_DATA_LEN as usize {
            return Err(InstructionError::InvalidRealloc);
        }
        // The resize can not exceed the per-transaction maximum
        let length_delta = (new_len as i64).saturating_sub(old_len as i64);
        if self.resize_delta.get().saturating_add(length_delta)
            > MAX_ACCOUNT_DATA_GROWTH_PER_TRANSACTION
        {
            return Err(InstructionError::MaxAccountsDataAllocationsExceeded);
        }
        Ok(())
    }

    pub fn try_borrow_mut(
        &self,
        index: IndexOfAccount,
    ) -> Result<AccountRefMut<'_>, InstructionError> {
        let borrow_counter = self
            .borrow_counters
            .get(index as usize)
            .ok_or(InstructionError::MissingAccount)?;
        borrow_counter.try_borrow_mut()?;

        // SAFETY: The borrow counter guarantees this is the only mutable borrow of this account.
        // The unwrap is safe because accounts.len() == borrow_counters.len(), so the missing
        // account error should have been returned above.
        let account = TransactionAccountViewMut {
            account: unsafe { &mut (*self.accounts.get(index as usize).unwrap().get()).1 },
        };

        Ok(AccountRefMut { account, borrow_counter })
    }

    pub fn try_borrow(&self, index: IndexOfAccount) -> Result<AccountRef<'_>, InstructionError> {
        let borrow_counter = self
            .borrow_counters
            .get(index as usize)
            .ok_or(InstructionError::MissingAccount)?;
        borrow_counter.try_borrow()?;

        // SAFETY: The borrow counter guarantees there are no mutable borrow of this account.
        // The unwrap is safe because accounts.len() == borrow_counters.len(), so the missing
        // account error should have been returned above.
        let keyed_account = unsafe { &*self.accounts.get(index as usize).unwrap().get() };

        let account = TransactionAccountView { account: &keyed_account.1 };

        Ok(AccountRef { account, borrow_counter })
    }

    pub(crate) fn add_lamports_delta(&self, balance: i128) -> Result<(), InstructionError> {
        let delta = self.lamports_delta.get();
        self.lamports_delta
            .set(delta.checked_add(balance).ok_or(InstructionError::ArithmeticOverflow)?);
        Ok(())
    }

    pub(crate) fn get_lamports_delta(&self) -> i128 {
        self.lamports_delta.get()
    }

    fn drain_accounts(&mut self) -> Box<[UnsafeCell<KeyedAccountSharedData>]> {
        debug_assert_eq!(self.accounts.len(), self.borrow_counters.len());
        debug_assert_eq!(self.accounts.len(), self.touched_flags.len());
        std::mem::take(&mut self.accounts)
    }

    fn deconstruct_into_keyed_account_shared_data(&mut self) -> Vec<KeyedAccountSharedData> {
        self.drain_accounts().into_iter().map(UnsafeCell::into_inner).collect()
    }

    pub(crate) fn deconstruct_into_account_shared_data(&mut self) -> Vec<AccountSharedData> {
        self.drain_accounts().into_iter().map(|cell| cell.into_inner().1).collect()
    }

    pub(crate) fn take(mut self) -> DeconstructedTransactionAccounts {
        let shared_data = self.deconstruct_into_keyed_account_shared_data();
        (shared_data, self.touched_flags, self.resize_delta)
    }

    pub fn resize_delta(&self) -> i64 {
        self.resize_delta.get()
    }

    pub(crate) fn account_key(&self, index: IndexOfAccount) -> Option<&Pubkey> {
        // SAFETY: We never modify an account key, so returning a reference to it is safe.
        unsafe { self.accounts.get(index as usize).map(|acc| &(*acc.get()).0) }
    }

    pub(crate) fn account_keys_iter(&self) -> impl Iterator<Item = &Pubkey> {
        // SAFETY: We never modify account keys, so returning an immutable reference to them is safe.
        unsafe { self.accounts.iter().map(|item| &(*item.get()).0) }
    }
}

#[derive(Default, Debug, Clone)]
#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
struct BorrowCounter {
    counter: Cell<i8>,
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl BorrowCounter {
    #[inline]
    fn is_writing(&self) -> bool {
        self.counter.get() < 0
    }

    #[inline]
    fn is_reading(&self) -> bool {
        self.counter.get() > 0
    }

    #[inline]
    fn try_borrow(&self) -> Result<(), InstructionError> {
        if self.is_writing() {
            return Err(InstructionError::AccountBorrowFailed);
        }

        if let Some(counter) = self.counter.get().checked_add(1) {
            self.counter.set(counter);
            return Ok(());
        }

        Err(InstructionError::AccountBorrowFailed)
    }

    #[inline]
    fn try_borrow_mut(&self) -> Result<(), InstructionError> {
        if self.is_writing() || self.is_reading() {
            return Err(InstructionError::AccountBorrowFailed);
        }

        self.counter.set(self.counter.get().saturating_sub(1));

        Ok(())
    }

    #[inline]
    fn release_borrow(&self) {
        self.counter.set(self.counter.get().saturating_sub(1));
    }

    #[inline]
    fn release_borrow_mut(&self) {
        self.counter.set(self.counter.get().saturating_add(1));
    }
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
pub struct AccountRef<'a> {
    account: TransactionAccountView<'a>,
    borrow_counter: &'a BorrowCounter,
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl Drop for AccountRef<'_> {
    fn drop(&mut self) {
        self.borrow_counter.release_borrow();
    }
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl<'a> Deref for AccountRef<'a> {
    type Target = TransactionAccountView<'a>;
    fn deref(&self) -> &Self::Target {
        &self.account
    }
}

#[derive(Debug)]
#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
pub struct AccountRefMut<'a> {
    account: TransactionAccountViewMut<'a>,
    borrow_counter: &'a BorrowCounter,
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl Drop for AccountRefMut<'_> {
    fn drop(&mut self) {
        self.borrow_counter.release_borrow_mut();
    }
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl<'a> Deref for AccountRefMut<'a> {
    type Target = TransactionAccountViewMut<'a>;
    fn deref(&self) -> &Self::Target {
        &self.account
    }
}

#[cfg(not(any(target_arch = "bpf", target_arch = "sbf")))]
impl DerefMut for AccountRefMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.account
    }
}

#[cfg(all(test, not(target_arch = "sbf"), not(target_arch = "bpf")))]
mod tests {
    use {
        crate::transaction_accounts::TransactionAccounts,
        solana_account::{
            AccountBuilder, AccountFieldPatch, AccountMode, AccountSharedData, DirtyMarkers,
            ReadableAccount, StateFlags, WritableAccount,
        },
        solana_instruction::error::InstructionError,
        solana_pubkey::Pubkey,
    };

    #[test]
    fn test_missing_account() {
        let accounts = vec![
            (
                Pubkey::new_unique(),
                AccountSharedData::new(2, 1, &Pubkey::new_unique()),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(2, 1, &Pubkey::new_unique()),
            ),
        ];

        let tx_accounts = TransactionAccounts::new(accounts);

        let res = tx_accounts.try_borrow(3);
        assert_eq!(res.err(), Some(InstructionError::MissingAccount));

        let res = tx_accounts.try_borrow_mut(3);
        assert_eq!(res.err(), Some(InstructionError::MissingAccount));
    }

    #[test]
    fn test_invalid_borrow() {
        let accounts = vec![
            (
                Pubkey::new_unique(),
                AccountSharedData::new(2, 1, &Pubkey::new_unique()),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(2, 1, &Pubkey::new_unique()),
            ),
        ];

        let tx_accounts = TransactionAccounts::new(accounts);

        // Two immutable borrows are valid
        {
            let acc_1 = tx_accounts.try_borrow(0);
            assert!(acc_1.is_ok());

            let acc_2 = tx_accounts.try_borrow(1);
            assert!(acc_2.is_ok());

            let acc_1_new = tx_accounts.try_borrow(0);
            assert!(acc_1_new.is_ok());

            assert_eq!(acc_1.unwrap().account, acc_1_new.unwrap().account);
        }

        // Two mutable borrows are invalid
        {
            let acc_1 = tx_accounts.try_borrow_mut(0);
            assert!(acc_1.is_ok());

            let acc_2 = tx_accounts.try_borrow_mut(1);
            assert!(acc_2.is_ok());

            let acc_1_new = tx_accounts.try_borrow_mut(0);
            assert_eq!(acc_1_new.err(), Some(InstructionError::AccountBorrowFailed));
        }

        // Mutable after immutable must fail
        {
            let acc_1 = tx_accounts.try_borrow(0);
            assert!(acc_1.is_ok());

            let acc_2 = tx_accounts.try_borrow(1);
            assert!(acc_2.is_ok());

            let acc_1_new = tx_accounts.try_borrow_mut(0);
            assert_eq!(acc_1_new.err(), Some(InstructionError::AccountBorrowFailed));
        }

        // Immutable after mutable must fail
        {
            let acc_1 = tx_accounts.try_borrow_mut(0);
            assert!(acc_1.is_ok());

            let acc_2 = tx_accounts.try_borrow_mut(1);
            assert!(acc_2.is_ok());

            let acc_1_new = tx_accounts.try_borrow(0);
            assert_eq!(acc_1_new.err(), Some(InstructionError::AccountBorrowFailed));
        }

        // Different scopes are good
        {
            let acc_1 = tx_accounts.try_borrow_mut(0);
            assert!(acc_1.is_ok());
        }

        {
            let acc_1 = tx_accounts.try_borrow_mut(0);
            assert!(acc_1.is_ok());
        }
    }

    #[test]
    fn too_many_borrows() {
        let accounts = vec![
            (
                Pubkey::new_unique(),
                AccountSharedData::new(2, 1, &Pubkey::new_unique()),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(2, 1, &Pubkey::new_unique()),
            ),
        ];

        let tx_accounts = TransactionAccounts::new(accounts);
        let mut borrows = Vec::new();
        for i in 0..129 {
            let acc = tx_accounts.try_borrow(1);
            if i < 127 {
                assert!(acc.is_ok());
                borrows.push(acc.unwrap());
            } else {
                assert_eq!(acc.err(), Some(InstructionError::AccountBorrowFailed));
            }
        }
    }

    #[test]
    fn preserves_account_shared_data_on_deconstruct() {
        let key = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let mut account = AccountBuilder::default()
            .lamports(23)
            .data(vec![1, 2, 3])
            .owner(owner)
            .mode(AccountMode::ReadOnly)
            .slot(41)
            .executable(true)
            .build::<AccountSharedData>();

        AccountFieldPatch::Mode(AccountMode::Ephemeral).apply(&mut account).unwrap();
        AccountFieldPatch::Slot(42).apply(&mut account).unwrap();
        account.set_flags(StateFlags::EXECUTABLE);
        AccountFieldPatch::DataAt { offset: 0, data: vec![4, 5] }
            .apply(&mut account)
            .unwrap();

        let expected_markers = account.markers().bits();
        let mut tx_accounts = TransactionAccounts::new(vec![(key, account)]);
        let mut accounts = tx_accounts.deconstruct_into_account_shared_data();
        let account = accounts.pop().unwrap();

        assert!(accounts.is_empty());
        assert!(account.is(AccountMode::Ephemeral));
        assert_eq!(account.slot(), 42);
        assert!(account.executable());
        assert_eq!(account.data(), &[4, 5, 3]);
        assert_eq!(account.markers().bits(), expected_markers);
    }

    #[test]
    fn mutable_view_updates_account_shared_data() {
        let key = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let new_owner = Pubkey::new_unique();
        let tx_accounts =
            TransactionAccounts::new(vec![(key, AccountSharedData::new(7, 2, &owner))]);

        {
            let mut account = tx_accounts.try_borrow_mut(0).unwrap();
            account.set_lamports(11);
            account.set_owner(new_owner);
            account.set_executable(true);
            account.resize(4, 9);
            assert_eq!(account.data(), &[0, 0, 9, 9]);
            account.set_data_from_slice(&[1, 2, 3]);
            account.extend_from_slice(&[4, 5]);
            account.data_as_mut_slice()[0] = 8;
        }

        let mut tx_accounts = tx_accounts;
        let mut accounts = tx_accounts.deconstruct_into_account_shared_data();
        let account = accounts.pop().unwrap();

        assert!(accounts.is_empty());
        assert_eq!(account.lamports(), 11);
        assert_eq!(account.owner(), &new_owner);
        assert!(account.executable());
        assert_eq!(account.data(), &[8, 2, 3, 4, 5]);
        assert!(account.markers().contains(DirtyMarkers::LAMPORTS));
        assert!(account.markers().contains(DirtyMarkers::OWNER));
        assert!(account.markers().contains(DirtyMarkers::FLAGS));
        assert!(account.markers().contains(DirtyMarkers::DATA));
    }
}
