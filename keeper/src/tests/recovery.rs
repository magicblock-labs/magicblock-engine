//! Startup seeding, corruption recovery

use std::fs;

use accountsdb::AccountEntry;
use ledger::schema::Signed;
use nucleus::testkit::{V42_ID, block, signed_view};
use solana_account::{ReadableAccount, testkit::delegated_account};
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_sdk_ids::{loader_v4, sysvar};
use solana_sysvar::{
    clock::Clock, epoch_schedule::EpochSchedule, rent::Rent, slot_hashes::SysvarId,
};
use solana_transaction_error::TransactionError;

use super::TestKeeper;
use crate::ResolvedTransaction;
use crate::testkit::{Dirs, archived_snapshot, corrupt, keeper_builder, seal_and_archive};

/// Proves startup seeds features, programs, and a no-warmup epoch schedule,
/// including sub-32-slot intervals and the zero-sealing fallback.
#[tokio::test]
async fn seeds_features_programs_and_sysvars() {
    for interval in [1, 7, 0] {
        let dirs = Dirs::default();
        let mut builder = keeper_builder(&dirs);
        builder.blockstore.superblock = interval;
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
            let acc = loader.read(&id, Clone::clone).unwrap().expect("feature account seeded");
            assert_eq!(acc.owner(), &solana_feature_gate_interface::ID);
            assert!(acc.lamports() >= rent.minimum_balance(acc.data().len()));
        }

        // The upgradeable program account carries its ELF verbatim, is executable,
        // owned by loader_v4 (not the BPF upgradeable loader), and rent-exempt.
        // Builtins are seeded through the same path with an executable native-loader
        // account, so they share this shape.
        let acc = loader.read(&program, Clone::clone).unwrap().expect("program seeded");
        assert!(acc.executable());
        assert_eq!(acc.owner(), &loader_v4::ID);
        assert_eq!(acc.data(), elf.as_slice());
        assert_eq!(acc.lamports(), rent.minimum_balance(elf.len()));

        // The Clock is seeded one slot ahead of the last block; a fresh ledger's last
        // block defaults to slot 0, so the clock starts at slot 1.
        let clock: Clock = loader
            .read(&Clock::id(), Clone::clone)
            .unwrap()
            .expect("clock seeded")
            .deserialize_data()
            .unwrap();
        assert_eq!(clock.slot, 1);
        let schedule = keeper.epoch_schedule();
        assert_eq!(
            schedule.slots_per_epoch,
            if interval == 0 { 432_000 } else { interval }
        );
        assert_eq!(
            schedule.leader_schedule_slot_offset,
            schedule.slots_per_epoch
        );
        assert!(!schedule.warmup);
        assert_eq!(
            (schedule.first_normal_slot, schedule.first_normal_epoch),
            (0, 0)
        );
        assert_eq!(clock, keeper.clock(keeper.blocks().latest()));
        let stored: EpochSchedule = loader
            .read(&EpochSchedule::id(), Clone::clone)
            .unwrap()
            .unwrap()
            .deserialize_data()
            .unwrap();
        assert_eq!(&stored, schedule);

        // Rent and EpochSchedule sysvars are present and sysvar-owned.
        for id in [Rent::id(), EpochSchedule::id()] {
            let acc = loader.read(&id, Clone::clone).unwrap().expect("sysvar seeded");
            assert_eq!(acc.owner(), &sysvar::ID);
        }
        drop(loader);
        keeper.close().await;
    }
}

/// Proves snapshot recovery preserves the schedule, repeated seals do not advance
/// epochs, and volatile resets leave the schedule intact. The newest archived
/// account state wins, and the saved corrupt tree is removed after validation.
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

    let schedule = keeper.epoch_schedule().clone();
    let clock = keeper.clock(keeper.blocks().latest());
    // First snapshot captures marker == 1.
    store_marker(&keeper, marker, 1);
    seal_and_archive(&keeper).await;
    assert!(
        archived_snapshot(&keeper).is_some(),
        "snapshot archived under superblock"
    );
    // Second snapshot, in a later superblock, captures marker == 2.
    store_marker(&keeper, marker, 2);
    seal_and_archive(&keeper).await;
    assert_eq!(
        keeper.clock(keeper.blocks().latest()),
        clock,
        "seals do not advance epochs"
    );
    keeper.apply_reset(nucleus::ledger::Reset(0)).unwrap();
    assert_eq!(
        keeper.epoch_schedule(),
        &schedule,
        "volatile reset retains the schedule"
    );
    // Past every archive: this value is what an un-restored store would keep.
    store_marker(&keeper, marker, 3);
    let dirs = keeper.close().await;

    // Corruption must follow the close, whose flush would otherwise republish a
    // valid checksum over the poisoned word.
    corrupt(dirs.accounts.path(), 8, 0xABAB_ABAB_ABAB_ABAB);

    let keeper = TestKeeper::from_builder(dirs, builder).await;
    keeper.accounts().validate().expect("restored store validates");
    assert_eq!(keeper.epoch_schedule(), &schedule);
    assert_eq!(keeper.clock(keeper.blocks().latest()), clock);
    let restored = keeper
        .accounts()
        .loader()
        .read(&marker, Clone::clone)
        .unwrap()
        .expect("marker restored");
    assert_eq!(restored.lamports(), 2, "newest snapshot wins");
    // The corrupt tree saved for inspection is removed on successful recovery.
    assert!(!keeper.dirs.accounts.path().join("CURRENT.bkp").exists());

    keeper.close().await;
}

/// Proves leaders retain one blockhash while followers restore history and deduplication.
#[tokio::test]
async fn restores_blockhash_history_from_ledger_and_snapshot() {
    let dirs = Dirs::default();
    let mut builder = keeper_builder(&dirs);
    let keeper = TestKeeper::from_builder(dirs, builder.clone()).await;
    for slot in 1..=600 {
        let block = Signed::new(block(slot), keeper.signer());
        keeper.blocks().append(block, false).unwrap();
    }
    let ledger_hash = block(50).hash;
    let snapshot_hash = block(100).hash;
    let recent_hash = block(600).hash;
    let payer = Keypair::new();
    let (signature, view) = signed_view(
        &payer,
        [Instruction::new_with_bytes(V42_ID, &[], vec![])],
        recent_hash,
    );
    let transaction =
        ResolvedTransaction::try_new(view, Some(Default::default()), &Default::default()).unwrap();
    keeper.transactions().append(&transaction).await.unwrap().unwrap();
    // Match the executor's committed-count update while deliberately omitting
    // execution metadata, so recovery can only find this signature in blockstore.
    keeper.accounts().commit(std::iter::empty::<&AccountEntry>()).unwrap();
    keeper.blocks().append(Signed::new(block(601), keeper.signer()), false).unwrap();
    let latest_hash = block(601).hash;
    keeper.accounts().dump(None).unwrap();
    let dirs = keeper.close().await;

    let keeper = TestKeeper::from_builder(dirs, builder.clone()).await;
    assert!(
        !keeper.blocks().is_valid(&ledger_hash),
        "leader rejects every retained hash except the latest"
    );
    assert!(keeper.blocks().is_valid(&latest_hash));
    keeper.accounts().dump(None).unwrap();
    let dirs = keeper.close().await;

    builder.authority.remote = Some(Pubkey::new_unique());
    let keeper = TestKeeper::from_builder(dirs, builder.clone()).await;
    assert!(
        keeper.blocks().is_valid(&ledger_hash),
        "follower restores history beyond SlotHashes from the blockstore"
    );
    let status = keeper.transactions().subscribe_signature(signature).await.unwrap();
    assert_eq!(
        keeper.transactions().append(&transaction).await.unwrap(),
        Err(TransactionError::AlreadyProcessed)
    );
    assert!(matches!(
        status.try_recv(),
        Err(oneshot::TryRecvError::Empty)
    ));
    assert!(
        keeper.transactions().status(signature).await.unwrap().is_none(),
        "recovered follower signature has no historical status"
    );
    keeper.accounts().dump(None).unwrap();
    let dirs = keeper.close().await;
    fs::remove_dir_all(dirs.ledger.path()).unwrap();
    fs::create_dir(dirs.ledger.path()).unwrap();

    let keeper = TestKeeper::from_builder(dirs, builder).await;
    assert!(
        !keeper.blocks().is_valid(&ledger_hash),
        "clean ledger cannot restore history older than SlotHashes"
    );
    assert!(keeper.blocks().is_valid(&snapshot_hash));
    let payer = Keypair::new();
    let (_, view) = signed_view(
        &payer,
        [Instruction::new_with_bytes(V42_ID, &[], vec![])],
        snapshot_hash,
    );
    let transaction =
        ResolvedTransaction::try_new(view, Some(Default::default()), &Default::default()).unwrap();
    assert!(
        keeper.transactions().append(&transaction).await.unwrap().is_ok(),
        "follower accepts a snapshot-retained hash without ledger history"
    );
    keeper.close().await;
}

/// Stores the recovery marker account at `lamports`, the value each snapshot
/// captures and recovery must bring back.
fn store_marker(keeper: &TestKeeper, marker: Pubkey, lamports: u64) {
    let account = delegated_account(lamports, vec![], Pubkey::default());
    keeper.accounts().store(&[(marker, account.build())]).unwrap();
}
