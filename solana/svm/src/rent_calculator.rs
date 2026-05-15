//! Solana SVM Rent Calculator.
//!
//! Rent management for SVM.

use {
    solana_account::{AccountMode, AccountSharedData, ReadableAccount},
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_transaction_context::{IndexOfAccount, transaction::TransactionContext},
    solana_transaction_error::{TransactionError, TransactionResult},
};

/// When rent is collected from an exempt account, rent_epoch is set to this
/// value. The idea is to have a fixed, consistent value for rent_epoch for all accounts that do not collect rent.
/// This enables us to get rid of the field completely.
pub const RENT_EXEMPT_RENT_EPOCH: Epoch = Epoch::MAX;

/// Rent state of a Solana account.
#[derive(Debug, PartialEq, Eq)]
pub enum RentState {
    /// account.lamports == 0
    Uninitialized,
    /// 0 < account.lamports < rent-exempt-minimum
    RentPaying {
        lamports: u64,    // account.lamports()
        data_size: usize, // account.data().len()
    },
    /// account.lamports >= rent-exempt-minimum
    RentExempt,
}

/// Checks a writable account rent-state transition inside a transaction.
pub fn check_rent_state(
    pre_rent_state: Option<&RentState>,
    post_rent_state: Option<&RentState>,
    transaction_context: &TransactionContext,
    index: IndexOfAccount,
) -> TransactionResult<()> {
    if let Some((pre_rent_state, post_rent_state)) = pre_rent_state.zip(post_rent_state) {
        let expect_msg = "account must exist at TransactionContext index if rent-states are Some";
        check_rent_state_with_account(
            pre_rent_state,
            post_rent_state,
            transaction_context.get_key_of_account_at_index(index).expect(expect_msg),
            index,
        )?;
    }
    Ok(())
}

/// Checks a rent-state transition for a known account address.
///
/// The incinerator is exempt from this check.
pub fn check_rent_state_with_account(
    pre_rent_state: &RentState,
    post_rent_state: &RentState,
    address: &Pubkey,
    account_index: IndexOfAccount,
) -> TransactionResult<()> {
    if !solana_sdk_ids::incinerator::check_id(address)
        && !transition_allowed(pre_rent_state, post_rent_state)
    {
        let account_index = account_index as u8;
        Err(TransactionError::InsufficientFundsForRent { account_index })
    } else {
        Ok(())
    }
}

/// Determines the rent state of an account from lamports and data size.
pub fn get_account_rent_state(rent: &Rent, acc: &AccountSharedData) -> RentState {
    let (lamports, len) = (acc.lamports(), acc.data().len());
    if lamports == 0 {
        RentState::Uninitialized
    } else if rent.is_exempt(lamports, len) || acc.is(AccountMode::Ephemeral) {
        RentState::RentExempt
    } else {
        RentState::RentPaying { data_size: len, lamports }
    }
}

/// Returns whether a pre/post rent-state transition is valid.
///
/// Any state may become uninitialized or rent-exempt. A rent-paying account
/// may remain rent-paying only if it keeps the same data size and is not
/// credited.
pub fn transition_allowed(pre_rent_state: &RentState, post_rent_state: &RentState) -> bool {
    match post_rent_state {
        RentState::Uninitialized | RentState::RentExempt => true,
        RentState::RentPaying {
            data_size: post_data_size,
            lamports: post_lamports,
        } => {
            match pre_rent_state {
                RentState::Uninitialized | RentState::RentExempt => false,
                RentState::RentPaying {
                    data_size: pre_data_size,
                    lamports: pre_lamports,
                } => {
                    // Cannot remain RentPaying if resized or credited.
                    post_data_size == pre_data_size && post_lamports <= pre_lamports
                }
            }
        }
    }
}
