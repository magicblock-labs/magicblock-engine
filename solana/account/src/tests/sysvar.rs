use {
    crate::{create_account_with_fields, from_account},
    solana_clock::{Clock, Epoch},
};

#[test]
fn test_create_account_with_fields_round_trips_sysvar() {
    let clock = Clock { epoch: 7, ..Clock::default() };

    let account = create_account_with_fields(&clock, (3, Epoch::MAX));

    assert_eq!(account.lamports, 3);
    assert_eq!(account.rent_epoch, Epoch::MAX);
    assert_eq!(from_account::<Clock, _>(&account), Some(clock));
}
