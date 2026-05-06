//! Keeper-level test harness shared by keeper and processor test suites.
//!
//! Builds a real [`Keeper`] over throwaway directories with the canonical test
//! parameters (retention disabled, 400 ms blocktime, superblock 16), and exposes
//! the loadable v42 calculator program guaranteed by `build.rs`. The low-level,
//! engine-agnostic builders (transactions, blocks, tempdirs) are re-exported from
//! [`nucleus::testkit`]. Compiled only under the `testkit` feature (or a crate's
//! own `cfg(test)`), so it never reaches release builds.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{
    collections::HashMap,
    fs::{self, File},
    num::NonZeroU64,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use accountsdb::{AccountsDB, STORAGE_FILE};
use derive_more::Deref;
use nucleus::{
    config::{AccountsDBParams, BlockstoreParams, LedgerParams},
    ledger::ACCOUNTSDB_SNAPSHOT_FILE,
    shutdown::ShutdownManager,
    testkit::signed_view as compose_view,
};
use solana_account::{AccountBuilder, AccountMode, ReadableAccount};
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_sysvar::rent::Rent;

pub use nucleus::testkit::{
    TempDir, V42_ID, WireVersion, block, init_tracing, patterned_bytes,
    sign_versioned_instructions, tempdir, transaction, v42_padded_value, v42_sum,
};
use tokio::time;

use crate::{Keeper, ResolvedTransaction, TransactionView, builder::KeeperBuilder};

/// The v42 calculator ELF, built and located by `keeper/build.rs`.
pub const V42_PROGRAM_ELF: &[u8] = include_bytes!(env!("V42_CALCULATOR_PROGRAM_SO"));
/// Slots sealed into each superblock by the standard test engine.
pub const SUPERBLOCK: NonZeroU64 = NonZeroU64::new(4).unwrap();

/// Throwaway on-disk homes for the accountsdb and ledger stores.
///
/// The directories must outlive every keeper opened over them — the stores keep
/// their files open/mmapped — which is why the recovery tests hold `Dirs` across
/// a full close-and-reopen cycle.
pub struct Dirs {
    /// Accounts database directory.
    pub accounts: TempDir,
    /// Ledger directory.
    pub ledger: TempDir,
}

impl Default for Dirs {
    fn default() -> Self {
        Self {
            accounts: tempdir(),
            ledger: tempdir(),
        }
    }
}

/// A keeper builder over `dirs` with retention disabled and a 100 ms blocktime.
///
/// `builtins` and `accounts` default to empty and `programs` holds only v42;
/// individual tests fill the rest as needed. [`TestKeeper::new`] is the seeded
/// path, adding a funded payer on top.
pub fn keeper_builder(dirs: &Dirs) -> KeeperBuilder {
    let mut programs = HashMap::new();
    programs.insert(V42_ID, V42_PROGRAM_ELF.to_vec());
    init_tracing();

    KeeperBuilder {
        authority: Keypair::new().into(),
        accountsdb: AccountsDBParams {
            directory: dirs.accounts.path().to_owned(),
            lru_capacity: 256,
        },
        ledger: LedgerParams {
            directory: dirs.ledger.path().to_owned(),
            size_limit: u64::MAX,
        },
        blockstore: BlockstoreParams {
            blocktime: Duration::from_millis(100),
            superblock: SUPERBLOCK,
        },
        builtins: Default::default(),
        programs,
        accounts: Default::default(),
        rent: Rent::default(),
    }
}

/// A built keeper together with the directories and shutdown manager keeping its
/// background services alive. Derefs to [`Keeper`] for accessor calls.
///
/// The keeper is seeded at construction with the v42 program and one funded
/// payer, so keeper-backed suites can build and run transactions immediately
/// instead of re-loading the ELF or re-storing a signer per test.
#[derive(Deref)]
pub struct TestKeeper {
    /// Directories backing this keeper, returned by [`Self::close`] so a test can
    /// reopen over the same on-disk state.
    pub dirs: Dirs,
    /// Lifecycle manager owning the keeper's background services; exposed so
    /// tests can register their own services against the same shutdown.
    pub shutdown: ShutdownManager,
    #[deref]
    keeper: Arc<Keeper>,
}

impl TestKeeper {
    /// Builds a keeper on fresh directories seeded with v42 and a funded payer.
    pub async fn new() -> Self {
        Self::with(Dirs::default()).await
    }

    /// [`Self::new`] over `dirs`, which may already hold state from an earlier
    /// keeper closed over the same directories.
    pub async fn with(dirs: Dirs) -> Self {
        let mut builder = keeper_builder(&dirs);
        let payer = Keypair::new();
        builder.accounts.insert(
            payer.pubkey(),
            AccountBuilder::default().lamports(1_000_000).build(),
        );
        Self::from_builder(dirs, builder).await
    }

    /// Builds a keeper from a caller-configured builder.
    ///
    /// `dirs` must own the directories referenced by `builder` and outlive the
    /// resulting keeper. Unlike [`Self::with`], nothing is seeded beyond what the
    /// builder already carries.
    pub async fn from_builder(dirs: Dirs, builder: KeeperBuilder) -> Self {
        let mut shutdown = ShutdownManager::default();
        let keeper = Arc::new(builder.build(&mut shutdown).await.unwrap());
        Self { dirs, shutdown, keeper }
    }

    /// Flushes durable state, stops every background service, and returns the
    /// directories for reopen.
    ///
    /// The flush republishes a valid accountsdb checksum, so a test that wants a
    /// corrupt store must call [`corrupt`] on the returned directories *after*
    /// this, never before.
    pub async fn close(self) -> Dirs {
        let Self { mut shutdown, keeper, dirs } = self;
        keeper.sync(true).unwrap();
        shutdown.terminate().await;
        dirs
    }
}

/// A signed, resolved no-op transaction and its first signature, built the same
/// way the sequencer resolves inbound transactions.
pub fn signed_tx() -> (Signature, ResolvedTransaction) {
    let (signature, bytes) = transaction(&[]);
    let view = TransactionView::try_new_sanitized(bytes, true).unwrap();
    let resolved =
        ResolvedTransaction::try_new(view, Some(Default::default()), &Default::default()).unwrap();
    (signature, resolved)
}

/// A resolved transaction whose account metadata matches `accounts`.
///
/// Each tuple is `(pubkey, writable)`. The transaction is fully sanitized and
/// resolved so scheduling sees the same account flags the keeper resolution path
/// would produce. A fresh random program id per call keeps the referenced account
/// set disjoint from other transactions under test.
pub fn resolved(accounts: &[(Pubkey, bool)]) -> ResolvedTransaction {
    let payer = Keypair::new();
    let program = Pubkey::new_unique();
    let metas = accounts
        .iter()
        .map(|(key, writable)| {
            if *writable {
                AccountMeta::new(*key, false)
            } else {
                AccountMeta::new_readonly(*key, false)
            }
        })
        .collect();
    let ix = Instruction::new_with_bytes(program, &[], metas);
    let (_signature, view) = compose_view(&payer, [ix], Hash::default());
    ResolvedTransaction::try_new(view, Some(Default::default()), &Default::default()).unwrap()
}

/// Configures a v42 account carrying an 8-byte little-endian `i64` and twice
/// its rent-exempt minimum, leaving one reserve available for transfer tests.
pub fn v42_builder(value: i64, mode: AccountMode) -> AccountBuilder {
    AccountBuilder::default()
        .lamports(Rent::default().minimum_balance(8) * 2)
        .owner(V42_ID)
        .mode(mode)
        .data(value.to_le_bytes().to_vec())
}

/// Stores a funded v42 `i64` account in `mode` and returns its pubkey.
pub fn store_v42(keeper: &Keeper, value: i64, mode: AccountMode) -> Pubkey {
    let key = Pubkey::new_unique();
    keeper.accounts().store(&[(key, v42_builder(value, mode).build())]).unwrap();
    key
}

/// Reads the little-endian `i64` payload of a v42 account, or `None` if absent.
pub fn load_v42_data(keeper: &Keeper, key: Pubkey) -> Option<i64> {
    keeper.accounts().loader().read(&key, decode_v42).unwrap()
}

/// Reads the lamport balance of a stored v42 account, or `None` if absent.
pub fn load_v42_lamports(keeper: &Keeper, key: Pubkey) -> Option<u64> {
    keeper.accounts().loader().read(&key, ReadableAccount::lamports).unwrap()
}

/// Signs `instruction` into the sanitized transaction view consumed by services.
pub fn signed_view(
    keeper: &Keeper,
    payer: Option<&Keypair>,
    instruction: Instruction,
) -> (Signature, TransactionView) {
    let payer = payer.unwrap_or(keeper.signer());
    compose_view(payer, [instruction], keeper.blockhash())
}

/// Returns the archived accountsdb snapshot path under any retained superblock,
/// or `None` when no superblock directory holds one yet.
pub fn archived_snapshot(keeper: &Keeper) -> Option<PathBuf> {
    fs::read_dir(&keeper.ledger.directory)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path().join(ACCOUNTSDB_SNAPSHOT_FILE))
        .find(|p| p.exists())
}

/// Waits until a subscribed detached snapshot archiver reports completion.
pub async fn await_archive(keeper: &Keeper) -> PathBuf {
    let mut rx = keeper.accounts().subscribe_snapshots();
    time::timeout(Duration::from_secs(8), rx.recv())
        .await
        .expect("snapshot archives in time")
        .unwrap()
}

/// Poisons the persisted checksum under `root` so the next open reports corruption.
///
/// The mmap-backed persisted store keeps its `DatabaseMeta` at the front of
/// `CURRENT/storage.db`; the `u64` format version sits at offset 0 and the
/// recorded checksum immediately after it at offset 8. Overwriting the checksum
/// word (leaving the version intact) makes it disagree with the recomputed value
/// on reopen — the exact `AccountsDBError::Corruption` the recovery path keys on,
/// rather than an `UnsupportedVersion` it does not handle.
///
/// This must be the last write to the store: `flush(true)` recomputes and
/// republishes the checksum, so this must run *after* the keeper is closed, never
/// against a live one.
pub fn corrupt(root: &Path) {
    use std::io::{Seek, SeekFrom, Write};
    let path = AccountsDB::directory(root).join(STORAGE_FILE);
    let mut file = File::options().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(8)).unwrap();
    file.write_all(&[0xAB; 8]).unwrap();
    file.flush().unwrap();
}

/// Decodes the little-endian `i64` payload stored in a v42 account.
pub fn decode_v42(account: &impl ReadableAccount) -> i64 {
    i64::from_le_bytes(account.data()[..8].try_into().expect("v42 account holds an i64"))
}
