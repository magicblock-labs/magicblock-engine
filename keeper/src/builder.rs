//! Keeper construction, recovery, and startup account seeding.

use std::{
    collections::HashMap,
    fs::{self, File},
    time::Duration,
};

use accountsdb::{AccountEntry, AccountsDB, AccountsDBError, BackupOp, SnapshotError};
use agave_feature_set::FeatureSet;
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
use serde::Serialize;
use solana_account::{AccountBuilder, AccountMode, AccountSharedData, ReadableAccount};
use solana_feature_gate_interface::Feature;
use solana_program_runtime::invoke_context::BuiltinFunctionWithContext;
use solana_pubkey::Pubkey;
use solana_sdk_ids::sysvar;
#[allow(deprecated)]
use solana_sysvar::fees::Fees;
use solana_sysvar::{
    clock::Clock,
    epoch_rewards::EpochRewards,
    epoch_schedule::EpochSchedule,
    last_restart_slot::LastRestartSlot,
    rent::Rent,
    slot_hashes::{SlotHashes, SysvarId},
};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::{
    Keeper, LEDGER_STREAM_CAPACITY,
    cache::Caches,
    error::{KeeperError, Result},
    metrics,
    subscriptions::Subscriptions,
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
    /// Native builtin program ids to seed as executable accounts.
    pub builtins: HashMap<Pubkey, BuiltinFunctionWithContext>,
    /// Upgradeable program accounts to seed, paired as `(program id, ELF bytes)`.
    pub programs: HashMap<Pubkey, Vec<u8>>,
    /// Plain accounts to seed into storage before startup completes.
    pub accounts: HashMap<Pubkey, AccountSharedData>,
    /// Rent parameters used to size seeded accounts and the Rent sysvar.
    pub rent: Rent,
}

impl KeeperBuilder {
    /// Open durable stores, recover accounts from the latest snapshot if needed, and wire caches.
    pub async fn build(mut self, shutdown: &mut ShutdownManager) -> Result<Keeper> {
        let ledger = Ledger::init(&self.ledger.directory, self.ledger.size_limit, shutdown)?;
        let accountsdb = self.accountsdb(&ledger)?;
        let (caches, featureset) = self.prepopulate(&accountsdb, &ledger).await?;
        metrics::init();
        Ok(Keeper {
            authority: self.authority,
            featureset,
            rent: self.rent,
            accountsdb,
            ledger,
            caches,
            subscriptions: Subscriptions::new(shutdown),
        })
    }

    /// Seeds accounts needed before the engine starts serving reads.
    async fn prepopulate(
        &mut self,
        accountsdb: &AccountsDB,
        ledger: &LedgerHandle,
    ) -> Result<(Caches, FeatureSet)> {
        let mut accounts = Vec::new();
        let featureset = self.seed_featureset(&mut accounts)?;
        self.seed_programs(&mut accounts)?;
        let caches = self.seed_sysvars(accountsdb, ledger, &mut accounts).await?;
        let authority = self.authority.pubkey();
        if accountsdb.loader().load(&authority)?.is_none() {
            let sponsor = AccountBuilder::default()
                .lamports(SPONSOR_INIT_BALANCE)
                .mode(AccountMode::Ephemeral);
            accounts.push((authority, sponsor.build()));
        }
        accounts.extend(self.accounts.drain());
        accountsdb.store(&accounts)?;
        Ok((caches, featureset))
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

    /// Activates the engine's required feature gates at slot 0, seeds a feature
    /// account for each, and returns the resulting [`FeatureSet`].
    fn seed_featureset(&self, accounts: &mut Vec<AccountEntry>) -> Result<FeatureSet> {
        let mut featureset = FeatureSet::default();
        [
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
        ]
        .iter()
        .for_each(|f| featureset.activate(f, 0));
        for (&id, &slot) in featureset.active() {
            let feature = &Feature { activated_at: Some(slot) };
            let account = self.account(feature, &solana_feature_gate_interface::ID)?;
            accounts.push((id, account.build()));
        }
        Ok(featureset)
    }

    /// Seeds builtin and upgradeable program accounts.
    fn seed_programs(&self, accounts: &mut Vec<AccountEntry>) -> Result<()> {
        for &builtin in self.builtins.keys() {
            let account = self.account(&(), &solana_sdk_ids::native_loader::ID)?;
            let account = account.executable(true).build();
            accounts.push((builtin, account));
        }

        for (&program, elf) in &self.programs {
            let lamports = self.rent.minimum_balance(elf.len());
            let account = AccountBuilder::default()
                .lamports(lamports)
                .mode(AccountMode::System)
                .owner(solana_sdk_ids::loader_v4::ID)
                .executable(true)
                .data(elf.clone());
            accounts.push((program, account.build()));
        }
        Ok(())
    }

    /// Seeds block-derived sysvars and builds the caches needed for this node's role.
    async fn seed_sysvars(
        &self,
        accountsdb: &AccountsDB,
        ledger: &LedgerHandle,
        accounts: &mut Vec<AccountEntry>,
    ) -> Result<Caches> {
        let slot = accountsdb.slot();
        let loader = accountsdb.loader();
        let id = SlotHashes::id();
        // AccountsDB starts at slot 1 and the ledger starts at head 1. Only that
        // pair is genesis; every later state must carry durable SlotHashes.
        let genesis = slot == 1 && ledger.head() == 1;
        let slothashes = match loader.load(&id)? {
            Some(account) => {
                account.deserialize_data::<SlotHashes>().map_err(AccountsDBError::from)?
            }
            None if genesis => {
                // Keep the sysvar account at its fixed serialized capacity so live
                // updates can replace entries without resizing the account.
                let hashes = SlotHashes::new(&[Default::default(); SLOTHASH_ENTRIES]);
                let account = self.account(&hashes, &sysvar::ID)?;
                accounts.push((id, account.build()));
                hashes
            }
            None => return Err(KeeperError::MissingSysvar(id)),
        };

        // A remote authority makes this engine a replication client. Followers
        // need retained dedup history; leaders deliberately retain only the tip
        // so they can restart without scanning the ledger.
        let caches = if self.authority.remote.is_some() {
            self.follower_caches(ledger, slot, &slothashes).await?
        } else {
            self.leader_caches(&slothashes)
        };

        // Set the clock slot one ahead from the last
        let latest = caches.latest();
        let clock = Clock {
            slot: latest.slot + 1,
            unix_timestamp: latest.time,
            ..Default::default()
        };
        accounts.push((Clock::id(), self.account(&clock, &sysvar::ID)?.build()));
        accounts.push((Rent::id(), self.account(&self.rent, &sysvar::ID)?.build()));
        #[allow(deprecated)]
        accounts.push((
            Fees::id(),
            self.account(&Fees::default(), &sysvar::ID)?.build(),
        ));
        accounts.push((
            sysvar::last_restart_slot::id(),
            self.account(&LastRestartSlot::default(), &sysvar::ID)?.build(),
        ));
        accounts.push((
            sysvar::instructions::id(),
            self.account(&(), &sysvar::ID)?.build(),
        ));
        accounts.push((
            EpochSchedule::id(),
            self.account(&EpochSchedule::default(), &sysvar::ID)?.build(),
        ));
        accounts.push((
            EpochRewards::id(),
            self.account(&EpochRewards::default(), &sysvar::ID)?.build(),
        ));
        Ok(caches)
    }

    /// Streams retained history into follower caches without materializing the range.
    async fn follower_caches(
        &self,
        ledger: &LedgerHandle,
        slot: Slot,
        slothashes: &SlotHashes,
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

        let caches = self.caches(persisted_latest(slothashes));
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
    fn leader_caches(&self, slothashes: &SlotHashes) -> Caches {
        let latest = persisted_latest(slothashes);
        let caches = self.caches(latest);
        caches.restore_hash(latest.slot, latest.hash);
        caches
    }

    /// Builds a rent-exempt system account containing a serialized sysvar-like state.
    fn account<S: Serialize>(&self, state: &S, owner: &Pubkey) -> Result<AccountBuilder> {
        let account =
            AccountSharedData::new_data(0, state, owner).map_err(AccountsDBError::from)?;
        let lamports = self.rent.minimum_balance(account.data().len());
        Ok(AccountBuilder::from(account).lamports(lamports).mode(AccountMode::System))
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
            let mut tar = tar::Archive::new(zstd::Decoder::new(file)?);
            tar.unpack(dst)?;
            info!(directory = ?superblock.directory, "restored accountsdb snapshot");
            return Ok(());
        }
        Err(SnapshotError::Missing.into())
    }
}

fn persisted_latest(slothashes: &SlotHashes) -> Block {
    let hashes = slothashes.slot_hashes();
    hashes.first().map_or(Block::default(), |&(slot, hash)| Block {
        slot,
        hash,
        time: 0,
        parent: hashes.get(1).map(|(_, hash)| *hash).unwrap_or_default(),
    })
}
