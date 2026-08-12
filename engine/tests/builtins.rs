//! Full-engine coverage for native builtins registered during startup.
#![cfg(test)]

use engine::testkit::TestEngine;
use solana_account::{AccountBuilder, AccountMode, AccountSharedData, ReadableAccount};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_system_interface::{
    instruction::{allocate, assign, transfer},
    program,
};
use solana_sysvar::rent::Rent;
use solana_transaction::Transaction;

#[tokio::test(flavor = "multi_thread")]
async fn system_program_executes_transfer_allocate_and_assign() {
    const LAMPORTS: u64 = 42;
    const SPACE: usize = 8;

    let te = TestEngine::new().await;
    let source = te.authority();

    let source_before = te.get_account(source).expect("authority account remains");
    assert_eq!(source_before.owner(), &program::ID);

    let destination = Keypair::new();
    let destination_before = Rent::default().minimum_balance(SPACE);
    let account: AccountSharedData = AccountBuilder::default()
        .lamports(destination_before)
        .mode(AccountMode::Delegated)
        .build();
    assert_eq!(account.owner(), &program::ID);
    te.accounts().store(&[(destination.pubkey(), account)]).unwrap();

    let owner = Pubkey::new_unique();
    let instructions = [
        transfer(&source, &destination.pubkey(), LAMPORTS),
        allocate(&destination.pubkey(), SPACE as u64),
        assign(&destination.pubkey(), &owner),
    ];
    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&source),
        &[te.signer(), &destination],
        te.blockhash(),
    );
    te.execute(transaction).await.expect("failed to execute system ixs");

    let source_after = te.get_account(source).expect("authority account remains");
    assert_eq!(source_after.lamports(), source_before.lamports() - LAMPORTS);
    assert_eq!(source_after.data(), source_before.data());
    assert_eq!(source_after.owner(), source_before.owner());

    let destination_after =
        te.get_account(destination.pubkey()).expect("destination account remains");
    assert_eq!(destination_after.lamports(), destination_before + LAMPORTS);
    assert_eq!(destination_after.data(), &[0; SPACE]);
    assert_eq!(destination_after.owner(), &owner);

    te.close().await;
}
