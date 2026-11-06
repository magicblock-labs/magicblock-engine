//! Keeper construction, recovery, and startup account seeding.

use std::{
    collections::HashMap,
    fs::{self, File},
    time::Duration,
};

use accountsdb::{
    AccountEntry, AccountLoader, AccountsDB, AccountsDBError, BackupOp, SnapshotError,
};
use agave_feature_set::FeatureSet;
use arc_swap::ArcSwap;
use ledger::{
    Ledger, LedgerHandle,
    request::{BlockRangeParams, ReadRequest, RequestPayload},
};
use nucleus::{
    Slot,
    config::{AccountsDBParams, Authority, BlockstoreParams, LedgerParams},
    ledger::{ACCOUNTSDB_SNAPSHOT_FILE, Block},
    shutdown::ShutdownManager,
};
use serde::de::DeserializeOwned;
use solana_account::{AccountBuilder, AccountMode, AccountSharedData};
use solana_program_runtime::invoke_context::BuiltinFunctionWithContext;
use solana_pubkey::Pubkey;
use solana_sdk_ids::sysvar;
use solana_sysvar::{
    clock::Clock,
    epoch_rewards::EpochRewards,
    epoch_schedule::EpochSchedule,
    fees,
    last_restart_slot::LastRestartSlot,
    rent::Rent,
    slot_hashes::{SlotHashes, SysvarId},
};
use tar::Archive;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use zstd::Decoder;

use crate::{
    Keeper, LEDGER_STREAM_CAPACITY,
    cache::Caches,
    error::{KeeperError, Result},
    feature_account, metrics, runtime_account,
    subscriptions::Subscriptions,
    sysvar_account,
};

/// Initial balance assigned to the authority account that sponsors account creation.
pub(crate) const SPONSOR_INIT_BALANCE: u64 = u64::MAX / 2;
/// Maximum number of recent hashes retained by the `SlotHashes` sysvar.
const SLOTHASH_ENTRIES: usize = 512;
/// Wall-clock retention window for recently processed signatures.
const SIGNATURE_CACHE_WINDOW: Duration = Duration::from_secs(75);
/// Wall-clock retention window for recently produced blocks.
const BLOCK_CACHE_WINDOW: Duration = Duration::from_secs(60);

/// Builder for keeper directories and cache timing.
#[derive(Clone)]
pub struct KeeperBuilder {
    /// Local signer and optional remote authority represented by the engine.
    pub authority: Authority,
    /// Accounts database storage parameters.
    pub accountsdb: AccountsDBParams,
    /// Ledger storage parameters.
    pub ledger: LedgerParams,
    /// Block production timing and superblock sealing parameters.
    pub blockstore: BlockstoreParams,
    /// Native builtin program ids to seed as executable runtime accounts if absent.
    pub builtins: HashMap<Pubkey, BuiltinFunctionWithContext>,
    /// SBF programs to seed if absent as loader-v4 accounts: `(program id, ELF bytes)`.
    pub programs: HashMap<Pubkey, Vec<u8>>,
    /// Accounts to seed at startup if missing from the database; existing accounts are kept.
    pub possibly_missing_accounts: HashMap<Pubkey, AccountSharedData>,
    /// Startup rent for a new leader; recovery uses the persisted Rent sysvar.
    pub rent: Rent,
}

impl KeeperBuilder {
    /// Open durable stores, recover accounts from the latest snapshot if needed, and wire caches.
    pub async fn build(mut self, shutdown: &mut ShutdownManager) -> Result<Keeper> {
        let ledger = Ledger::init(&self.ledger.directory, self.ledger.size_limit, shutdown)?;
        let accountsdb = self.accountsdb(&ledger)?;
        accountsdb.cleanup_snapshots()?;
        self.prepopulate(&accountsdb)?;
        let featureset = Self::restore_features(&accountsdb)?;
        let (rent, epoch_schedule, slothashes, clock) = {
            let loader = accountsdb.loader();
            (
                load_sysvar::<Rent>(&loader)?,
                load_sysvar::<EpochSchedule>(&loader)?,
                load_sysvar::<SlotHashes>(&loader)?,
                load_sysvar::<Clock>(&loader)?,
            )
        };
        let caches = if self.authority.remote.is_some() {
            self.follower_caches(&ledger, accountsdb.slot(), &slothashes, &clock).await?
        } else {
            self.leader_caches(&slothashes, &clock)
        };
        metrics::init();
        Ok(Keeper {
            authority: self.authority,
            featureset: ArcSwap::from_pointee(featureset),
            rent: rent.lamports_per_byte.into(),
            epoch_schedule,
            accountsdb,
            ledger,
            caches,
            subscriptions: Subscriptions::new(shutdown),
        })
    }

    /// Initializes runtime configuration once and seeds missing programs and accounts.
    fn prepopulate(&mut self, accountsdb: &AccountsDB) -> Result<()> {
        let mut accounts = Vec::new();
        if accountsdb.is_new() {
            for id in DEFAULT_FEATURES {
                accounts.push((id, feature_account()));
            }
            self.seed_sysvars(&mut accounts);
        }
        self.seed_programs();
        let loader = accountsdb.loader();
        let authority = self.authority.pubkey();
        if !loader.contains(&authority)? {
            let sponsor = AccountBuilder::default()
                .lamports(SPONSOR_INIT_BALANCE)
                .mode(AccountMode::Magic);
            accounts.push((authority, sponsor.build()));
        }
        for (pubkey, account) in self.possibly_missing_accounts.drain() {
            if !loader.contains(&pubkey)? {
                accounts.push((pubkey, account));
            }
        }
        drop(loader);
        accountsdb.store(&accounts).map_err(Into::into)
    }

    /// Epochs follow the local sealing interval; zero uses Solana's no-warmup default.
    fn epoch_schedule(&self) -> EpochSchedule {
        let mut schedule = EpochSchedule::without_warmup();
        // Direct assignment supports intervals below Solana's 32-slot minimum.
        if self.blockstore.superblock != 0 {
            schedule.slots_per_epoch = self.blockstore.superblock;
            schedule.leader_schedule_slot_offset = self.blockstore.superblock;
        }
        schedule
    }

    /// Builds read-side caches using blocktime-derived slot TTLs.
    fn caches(&self, latest: Block) -> Caches {
        Caches::new(
            latest,
            self.ttl(BLOCK_CACHE_WINDOW),
            self.ttl(SIGNATURE_CACHE_WINDOW),
            self.accountsdb.lru_capacity,
        )
    }

    /// Converts a wall-clock cache window into whole configured block slots.
    fn ttl(&self, window: Duration) -> Slot {
        window.div_duration_f64(self.blockstore.blocktime).ceil() as Slot
    }

    /// Reconstructs feature membership from authoritative runtime accounts.
    fn restore_features(accountsdb: &AccountsDB) -> Result<FeatureSet> {
        let mut features = FeatureSet::default();
        let accounts = accountsdb.program(&solana_feature_gate_interface::ID, |_, account| {
            account.mode()
        })?;
        for (id, _) in accounts.filter(|(_, mode)| *mode == AccountMode::Runtime) {
            features.activate(&id, 0);
        }
        Ok(features)
    }

    /// Adds program images to the common insert-if-missing startup seeds.
    fn seed_programs(&mut self) {
        let precompiles = [
            solana_sdk_ids::ed25519_program::ID,
            solana_sdk_ids::secp256k1_program::ID,
            solana_sdk_ids::secp256r1_program::ID,
        ];
        let builtins = self
            .builtins
            .keys()
            .copied()
            .chain(precompiles)
            .map(|id| (id, solana_sdk_ids::native_loader::ID, Vec::new()));
        let programs =
            self.programs.drain().map(|(id, elf)| (id, solana_sdk_ids::loader_v4::ID, elf));
        for (program, owner, elf) in builtins.chain(programs) {
            self.possibly_missing_accounts.entry(program).or_insert_with(|| {
                let lamports = Rent::default().minimum_balance(elf.len());
                AccountBuilder::default()
                    .lamports(lamports)
                    .mode(AccountMode::Runtime)
                    .owner(owner)
                    .executable(true)
                    .data(elf)
                    .build()
            });
        }
    }

    /// Creates the genesis sysvars; subsequent startup always reads persisted state.
    fn seed_sysvars(&self, accounts: &mut Vec<AccountEntry>) {
        let hashes = SlotHashes::new(&[Default::default(); SLOTHASH_ENTRIES]);
        let schedule = self.epoch_schedule();
        let clock = crate::clock(&schedule, Block::default());
        accounts.extend([
            sysvar_account(&hashes),
            sysvar_account(&clock),
            sysvar_account(&self.rent),
            #[allow(deprecated)]
            sysvar_account(&fees::Fees::default()),
            sysvar_account(&LastRestartSlot::default()),
            (
                sysvar::instructions::ID,
                runtime_account(&(), &sysvar::ID).build(),
            ),
            sysvar_account(&schedule),
            sysvar_account(&EpochRewards::default()),
        ]);
    }

    /// Streams retained history into follower caches without materializing the range.
    async fn follower_caches(
        &self,
        ledger: &LedgerHandle,
        slot: Slot,
        slothashes: &SlotHashes,
        clock: &Clock,
    ) -> Result<Caches> {
        let retained = self
            .ttl(BLOCK_CACHE_WINDOW)
            .max(self.ttl(SIGNATURE_CACHE_WINDOW))
            .max(SLOTHASH_ENTRIES as Slot);
        let (tx, mut rx) = mpsc::channel(LEDGER_STREAM_CAPACITY);
        let params = BlockRangeParams {
            range: slot.saturating_sub(retained - 1)..slot.saturating_add(1),
            tx,
        };
        let (payload, response) = RequestPayload::new(params);
        ledger.reader.send(ReadRequest::BlockRange(payload))?;

        let caches = self.caches(persisted_latest(slothashes, clock));
        // Merge the two ascending streams so ExpiringCache's eviction queue
        // stays ordered, preferring blockstore data for overlapping slots.
        let mut persisted = slothashes.slot_hashes().iter().rev().copied().peekable();
        while let Some(entry) = rx.recv().await {
            while let Some((slot, hash)) = persisted.next_if(|(slot, _)| *slot < entry.block.slot) {
                caches.restore_hash(slot, hash);
            }
            let _ = persisted.next_if(|(slot, _)| *slot == entry.block.slot);
            caches.restore(entry);
        }
        response.recv().await??;
        for (slot, hash) in persisted {
            caches.restore_hash(slot, hash);
        }
        Ok(caches)
    }

    /// Restores only AccountsDB's latest boundary so replay can advance it from there.
    fn leader_caches(&self, slothashes: &SlotHashes, clock: &Clock) -> Caches {
        let latest = persisted_latest(slothashes, clock);
        let caches = self.caches(latest);
        caches.restore_hash(latest.slot, latest.hash);
        caches
    }

    /// Opens accountsdb, restoring the newest archived snapshot after corruption.
    ///
    /// A restored store trails the ledger tip — snapshots are archived at sealed
    /// superblocks, not at the tip — so the returned accountsdb is only
    /// guaranteed to validate, not to be current. Catching it back up is the
    /// caller's job.
    fn accountsdb(&self, ledger: &LedgerHandle) -> Result<AccountsDB> {
        let mut backup = None;
        loop {
            let mut accountsdb = AccountsDB::new(&self.accountsdb.directory)?;
            // Seal N opens ledger head N+1, so accountsdb is current at head-1.
            let expected = ledger.head().saturating_sub(1);
            let restored = backup.is_some();
            let lagging = accountsdb.superblock() < expected;
            let count_lagging = accountsdb.transactions() < ledger.transactions();
            match accountsdb.validate() {
                Ok(()) if restored || (!lagging && !count_lagging) => {
                    let reclaimed = accountsdb.compact()?;
                    info!(
                        lagging,
                        count_lagging, reclaimed, "accountsdb validation succeeded"
                    );
                    backup.map(fs::remove_dir_all).transpose()?;
                    return Ok(accountsdb);
                }
                validation @ (Err(AccountsDBError::Corruption) | Ok(())) => {
                    if restored {
                        error!(?validation, "restored accountsdb is corrupt");
                        accountsdb.backup(BackupOp::Restore)?;
                        return Err(SnapshotError::Missing.into());
                    }
                    warn!(
                        ?validation,
                        lagging, count_lagging, "state inconsistency detected"
                    );
                    backup.replace(accountsdb.backup(BackupOp::Save)?);
                    if let Err(error) = self.unarchive(ledger) {
                        accountsdb.backup(BackupOp::Restore)?;
                        return Err(error);
                    }
                }
                Err(other) => return Err(other.into()),
            }
        }
    }

    /// Restores the first retained accountsdb snapshot found, from newest to oldest.
    fn unarchive(&self, ledger: &LedgerHandle) -> Result<()> {
        info!("restoring accountsdb from latest available snapshot");
        for superblock in ledger.iter() {
            let src = superblock.directory.join(ACCOUNTSDB_SNAPSHOT_FILE);
            if !src.exists() {
                continue;
            }
            let dst = AccountsDB::directory(&self.accountsdb.directory);
            let file = File::open(src)?;
            let mut tar = Archive::new(Decoder::new(file)?);
            tar.unpack(dst)?;
            info!(directory = ?superblock.directory, "restored accountsdb snapshot");
            return Ok(());
        }
        Err(SnapshotError::Missing.into())
    }
}

fn persisted_latest(slothashes: &SlotHashes, clock: &Clock) -> Block {
    let hashes = slothashes.slot_hashes();
    hashes.first().map_or(Block::default(), |&(slot, hash)| Block {
        slot,
        hash,
        time: clock.unix_timestamp,
        parent: hashes.get(1).map(|(_, hash)| *hash).unwrap_or_default(),
    })
}

/// Engine-selected features published by a leader at startup.
pub(crate) const DEFAULT_FEATURES: [Pubkey; 10] = [
    agave_feature_set::curve25519_syscall_enabled::ID,
    agave_feature_set::curve25519_restrict_msm_length::ID,
    agave_feature_set::enable_poseidon_syscall::ID,
    agave_feature_set::enable_sbpf_v3_deployment_and_execution::ID,
    agave_feature_set::virtual_address_space_adjustments::ID,
    agave_feature_set::syscall_parameter_address_restrictions::ID,
    agave_feature_set::get_sysvar_syscall_enabled::ID,
    agave_feature_set::ed25519_program_enabled::ID,
    agave_feature_set::secp256k1_program_enabled::ID,
    agave_feature_set::enable_secp256r1_precompile::ID,
];

fn load_sysvar<S: DeserializeOwned + SysvarId>(loader: &AccountLoader<'_>) -> Result<S> {
    loader
        .read(&S::id(), |account| account.deserialize_data::<S>())?
        .ok_or(KeeperError::MissingSysvar(S::id()))?
        .map_err(AccountsDBError::from)
        .map_err(Into::into)
}
