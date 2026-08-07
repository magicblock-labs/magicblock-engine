//! Startup seeding, corruption recovery

use solana_account::{AccountBuilder, AccountMode, ReadableAccount};
use solana_pubkey::Pubkey;
use solana_sdk_ids::{loader_v4, sysvar};
use solana_sysvar::{
    clock::Clock, epoch_schedule::EpochSchedule, rent::Rent, slot_hashes::SysvarId,
};

use super::TestKeeper;
use crate::testkit::{Dirs, archived_snapshot, await_archive, corrupt, keeper_builder};

// Startup seeds the engine's required feature gates, the configured upgradeable
// programs, and the sysvars, with the exact ownership/rent/clock-offset shape the
// rest of the engine assumes.
#[tokio::test]
async fn seeds_features_programs_and_sysvars() {
    let dirs = Dirs::default();
    let mut builder = keeper_builder(&dirs);
    let program = Pubkey::new_unique();
    let elf = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
    builder.programs.insert(program, elf.clone());
    let keeper = TestKeeper::from_builder(dirs, builder).await;
    let rent = Rent::default();
    let accounts = keeper.accounts();
    let loader = accounts.loader();

    // The engine's required curve25519/precompile/sbpf/sysvar gates are all
    // active at slot 0, and every active feature is backed by a rent-exempt
    // feature-gate-owned account.
    let required = [
        agave_feature_set::curve25519_syscall_enabled::ID,
        agave_feature_set::enable_sbpf_v3_deployment_and_execution::ID,
        agave_feature_set::syscall_parameter_address_restrictions::ID,
        agave_feature_set::get_sysvar_syscall_enabled::ID,
        agave_feature_set::ed25519_program_enabled::ID,
        agave_feature_set::secp256k1_program_enabled::ID,
    ];
    for id in required {
        assert_eq!(
            keeper.features().active().get(&id),
            Some(&0),
            "required gate active at slot 0"
        );
    }
    for (&id, &slot) in keeper.features().active() {
        assert_eq!(slot, 0, "features activate at slot 0");
        let acc = loader.load(&id).unwrap().expect("feature account seeded");
        assert_eq!(acc.owner(), &solana_feature_gate_interface::ID);
        assert!(acc.lamports() >= rent.minimum_balance(acc.data().len()));
    }

    // The upgradeable program account carries its ELF verbatim, is executable,
    // owned by loader_v4 (not the BPF upgradeable loader), and rent-exempt.
    // Builtins are seeded through the same path with an executable native-loader
    // account, so they share this shape.
    let acc = loader.load(&program).unwrap().expect("program seeded");
    assert!(acc.executable());
    assert_eq!(acc.owner(), &loader_v4::ID);
    assert_eq!(acc.data(), elf.as_slice());
    assert_eq!(acc.lamports(), rent.minimum_balance(elf.len()));

    // The Clock is seeded one slot ahead of the last block; a fresh ledger's last
    // block defaults to slot 0, so the clock starts at slot 1.
    let clock: Clock = loader
        .load(&Clock::id())
        .unwrap()
        .expect("clock seeded")
        .deserialize_data()
        .unwrap();
    assert_eq!(clock.slot, 1);

    // Rent and EpochSchedule sysvars are present and sysvar-owned.
    for id in [Rent::id(), EpochSchedule::id()] {
        let acc = loader.load(&id).unwrap().expect("sysvar seeded");
        assert_eq!(acc.owner(), &sysvar::ID);
    }
    drop(loader);
    keeper.close().await;
}

// A corrupt accountsdb on open is restored from the newest archived snapshot,
// and the saved corrupt tree is discarded once the restored store revalidates.
//
// The marker takes a distinct value in each state the reopen could land on, so
// the assertion separates all three: 1 is the older snapshot, 2 the newest, and
// 3 lives only in persisted state (stored after the last archive, so no snapshot
// holds it). Recovery must yield 2 — reading 3 back would mean the corruption
// went undetected and nothing was restored at all.
#[tokio::test]
async fn recovers_the_newest_snapshot() {
    let marker = Pubkey::new_unique();
    let dirs = Dirs::default();
    let builder = keeper_builder(&dirs);
    let keeper = TestKeeper::from_builder(dirs, builder.clone()).await;

    // First snapshot captures marker == 1.
    store_marker(&keeper, marker, 1);
    keeper.finalize_superblock().expect("first finalize");
    await_archive(&keeper).await;
    assert!(
        archived_snapshot(&keeper).is_some(),
        "snapshot archived under superblock"
    );
    // Second snapshot, in a later superblock, captures marker == 2.
    store_marker(&keeper, marker, 2);
    keeper.finalize_superblock().expect("second finalize");
    await_archive(&keeper).await;
    // Past every archive: this value is what an un-restored store would keep.
    store_marker(&keeper, marker, 3);
    let dirs = keeper.close().await;

    // Corruption must follow the close, whose flush would otherwise republish a
    // valid checksum over the poisoned word.
    corrupt(dirs.accounts.path(), 8, 0xABAB_ABAB_ABAB_ABAB);

    let keeper = TestKeeper::from_builder(dirs, builder).await;
    keeper.accounts().validate().expect("restored store validates");
    let restored = keeper.accounts().loader().load(&marker).unwrap().expect("marker restored");
    assert_eq!(restored.lamports(), 2, "newest snapshot wins");
    // The corrupt tree saved for inspection is removed on successful recovery.
    assert!(!keeper.dirs.accounts.path().join("CURRENT.bkp").exists());

    keeper.close().await;
}

/// Stores the recovery marker account at `lamports`, the value each snapshot
/// captures and recovery must bring back.
fn store_marker(keeper: &TestKeeper, marker: Pubkey, lamports: u64) {
    let account = AccountBuilder::default().lamports(lamports).mode(AccountMode::Delegated);
    keeper.accounts().store(&[(marker, account.build())]).unwrap();
}
