use {
    crate::{AccountSharedData, state_traits::StateMut},
    solana_instruction_error::InstructionError,
    solana_pubkey::Pubkey,
};

#[test]
fn test_account_state() {
    let state = 42;
    assert!(AccountSharedData::default().set_state(&state).is_err());
    let res = AccountSharedData::default().state() as Result<u64, InstructionError>;
    assert!(res.is_err());

    let mut account = AccountSharedData::new(0, size_of::<u64>(), &Pubkey::default());

    assert!(account.set_state(&state).is_ok());
    assert_eq!(account.state(), Ok(state));
}
