//! Fjall index schema and codecs for ledger blockstore entries.

use std::{
    iter::Rev,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use fjall::{
    CompressionType, Database, Keyspace, KeyspaceCreateOptions, OwnedWriteBatch, PersistMode,
    Readable, Snapshot,
    config::{CompressionPolicy, FilterPolicy},
    util::prefixed_range,
};
use nucleus::{MB, Slot};
use parking_lot::Mutex;
use solana_pubkey::Pubkey;
use solana_signature::Signature;

use crate::{
    error::{LedgerError, Result},
    schema::Offset,
    storage::Durability,
};

/// Index directory below each superblock directory.
const INDEX_SUBDIR: &str = "index";
/// Transaction signature index name.
const TRANSACTIONS_INDEX: &str = "transactions";
/// Slot-to-offset index name.
const SLOTS_INDEX: &str = "slots";
/// Account-to-offset index name.
const ACCOUNTS_INDEX: &str = "accounts";
/// Bytes kept from wide keys in compact index keys.
const PREFIX_BYTES: usize = 16;
/// Bytes occupied by one encoded span.
const SPAN_BYTES: usize = size_of::<u64>();
/// Bytes occupied by a transaction's two spans.
const TX_SPAN_BYTES: usize = 2 * SPAN_BYTES;
/// Bytes occupied by an account prefix and ordered execution span.
const ACCOUNT_KEY_BYTES: usize = PREFIX_BYTES + SPAN_BYTES;
/// Block cache capacity for one opened superblock index.
const CACHE_SIZE: u64 = 8 * MB as u64;
/// Fjall maintenance workers assigned to the active writable index.
const ACTIVE_WORKERS: usize = 2;
/// Fjall maintenance workers assigned to an on-demand sealed index.
const SEALED_WORKERS: usize = 1;

/// Truncated signature or account key.
///
/// The 16-byte prefix is an index tag, not a collision-proof identity. The
/// index accepts the negligible collision risk to keep keys compact.
type Prefix = [u8; PREFIX_BYTES];

/// Packed span inside a ledger data file.
///
/// The high 39 bits store the byte offset. The low 25 bits store the encoded
/// entry size. Values use little-endian encoding; spans embedded in ordered
/// keys use big-endian so Fjall's byte ordering matches numeric ordering.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Span(u64);

impl Span {
    /// Number of low bits reserved for entry size.
    const SIZE_BITS: u64 = 25;
    /// Mask for the encoded entry size.
    const SIZE_MASK: u64 = (1 << Self::SIZE_BITS) - 1;
    /// Largest blockstore entry size that can be packed into an index value.
    pub(crate) const MAX_SIZE: u64 = Self::SIZE_MASK;
    /// Largest file size addressable by the packed offset.
    pub(crate) const MAX_FILE_SIZE: u64 = (u64::MAX >> Self::SIZE_BITS) + 1;

    /// Packs an `offset` and `size` into one integer.
    pub(crate) fn new(offset: Offset, size: u64) -> Self {
        Self(offset << Self::SIZE_BITS | size)
    }

    /// Returns the byte offset in the backing file.
    pub(crate) fn offset(&self) -> u64 {
        self.0 >> Self::SIZE_BITS
    }

    /// Returns the encoded entry size in bytes.
    pub(crate) fn size(&self) -> u64 {
        self.0 & Self::SIZE_MASK
    }

    /// Encodes an opaque index value using the ledger's little-endian format.
    fn value_bytes(self) -> [u8; SPAN_BYTES] {
        self.0.to_le_bytes()
    }

    /// Encodes a numeric key component preserving its natural ordering.
    fn key_bytes(self) -> [u8; SPAN_BYTES] {
        self.0.to_be_bytes()
    }

    /// Decodes a little-endian span stored as an opaque value.
    fn from_value(bytes: &[u8]) -> Result<Self> {
        let value = fixed(bytes, "invalid span value")?;
        Ok(Self(u64::from_le_bytes(value)))
    }

    /// Decodes a big-endian span embedded in an ordered key.
    fn from_key(bytes: &[u8]) -> Result<Self> {
        let key = fixed(bytes, "invalid span key")?;
        Ok(Self(u64::from_be_bytes(key)))
    }
}

/// Pair of blockstore-file and execution-file spans for a transaction.
#[derive(Clone, Copy)]
pub(crate) struct TxSpan {
    /// Span of the raw transaction entry in the blockstore.
    pub(crate) blockstore: Span,
    /// Span of the execution details.
    pub(crate) execution: Span,
}

/// Reverse account-index iterator over execution spans.
pub(crate) struct AccountIter {
    inner: Rev<fjall::Iter>,
}

/// Reverse block-index iterator over `(slot, span)` pairs.
pub(crate) struct BlockIter {
    inner: Rev<fjall::Iter>,
}

impl Iterator for BlockIter {
    type Item = Result<(Slot, Span)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|entry| {
            let (key, value) = entry.into_inner()?;
            let key = fixed(&key, "invalid slot key")?;
            Ok((u64::from_be_bytes(key), Span::from_value(&value)?))
        })
    }
}

impl Iterator for AccountIter {
    type Item = Result<Span>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|entry| {
            let key = entry.key()?;
            key.get(PREFIX_BYTES..)
                .ok_or(LedgerError::Corruption("invalid account index key"))
                .and_then(Span::from_key)
        })
    }
}

/// Databases used to locate ledger data inside one superblock.
pub(crate) struct Index {
    /// Signature prefix to transaction and execution spans.
    transactions: Keyspace,
    /// Slot to blockstore span.
    blocks: Keyspace,
    /// Account prefix and execution span keys.
    accounts: Keyspace,
    /// Owning Fjall database.
    db: Database,
}

/// Atomic index mutations accumulated for one published ledger boundary.
pub(crate) struct IndexWriter {
    /// Index receiving the pending mutations.
    index: Arc<Index>,
    /// Mutations pending publication at the next boundary.
    batch: OwnedWriteBatch,
}

/// Consistent read snapshot that keeps its underlying index lease alive.
pub(crate) struct IndexReader {
    /// Index lease retained for the snapshot lifetime.
    index: Arc<Index>,
    /// Consistent view across every index keyspace.
    snapshot: Snapshot,
}

/// Lazy per-superblock index slot with lease-safe eviction.
pub(crate) struct IndexSlot {
    /// Superblock directory containing the index.
    directory: PathBuf,
    /// Synchronized active and cached state.
    state: Mutex<IndexState>,
}

/// Mutable state of a lazy index slot.
struct IndexState {
    /// Whether this index belongs to the writable head.
    active: bool,
    /// Open index, absent until a sealed index is first read.
    cached: Option<CachedIndex>,
}

/// Open index and its most recent lease time.
struct CachedIndex {
    /// Shared index lease.
    index: Arc<Index>,
    /// Most recent call to [`IndexSlot::get`].
    used: Instant,
}

impl Index {
    /// Opens or creates an index using `workers` Fjall maintenance threads.
    pub(crate) fn new(path: &Path, workers: usize) -> Result<Self> {
        let db = Database::builder(path.join(INDEX_SUBDIR))
            .cache_size(CACHE_SIZE)
            .worker_threads(workers)
            .manual_journal_persist(true)
            .journal_compression(CompressionType::None)
            .open()?;
        let options = || {
            KeyspaceCreateOptions::default()
                .data_block_compression_policy(CompressionPolicy::disabled())
                .index_block_compression_policy(CompressionPolicy::disabled())
                .filter_policy(FilterPolicy::disabled())
        };
        let transactions = db.keyspace(TRANSACTIONS_INDEX, options)?;
        let blocks = db.keyspace(SLOTS_INDEX, options)?;
        let accounts = db.keyspace(ACCOUNTS_INDEX, options)?;
        Ok(Self {
            db,
            transactions,
            blocks,
            accounts,
        })
    }

    /// Creates the single writer for this superblock index.
    pub(crate) fn writer(self: Arc<Self>) -> IndexWriter {
        let batch = self.db.batch();
        IndexWriter { index: self, batch }
    }

    /// Opens a consistent read view across all logical keyspaces.
    pub(crate) fn reader(self: Arc<Self>) -> IndexReader {
        let snapshot = self.db.snapshot();
        IndexReader { index: self, snapshot }
    }
}

impl IndexSlot {
    /// Creates a lazy index slot with its superblock lifecycle state.
    pub(crate) fn new(directory: &Path, active: bool) -> Self {
        Self {
            directory: directory.to_owned(),
            state: Mutex::new(IndexState { active, cached: None }),
        }
    }

    /// Opens or leases this superblock's index.
    pub(crate) fn get(&self) -> Result<Arc<Index>> {
        let mut state = self.state.lock();
        if let Some(cached) = &mut state.cached {
            cached.used = Instant::now();
            return Ok(cached.index.clone());
        }
        let workers = if state.active { ACTIVE_WORKERS } else { SEALED_WORKERS };
        let index = Arc::new(Index::new(&self.directory, workers)?);
        state.cached = Some(CachedIndex::new(index.clone()));
        Ok(index)
    }

    /// Marks the former writable index as sealed and cache-eligible.
    pub(crate) fn seal(&self) {
        let mut state = self.state.lock();
        state.active = false;
        if let Some(cached) = &mut state.cached {
            cached.used = Instant::now();
        }
    }

    /// Returns the last use of an opened sealed index.
    pub(crate) fn last_used(&self) -> Option<Instant> {
        let state = self.state.lock();
        if state.active {
            return None;
        }
        state.cached.as_ref().map(|cached| cached.used)
    }

    /// Closes the index when no external reader or writer lease remains.
    pub(crate) fn evict(&self) -> bool {
        let mut state = self.state.lock();
        if state.active {
            return false;
        }
        let Some(cached) = &state.cached else {
            return true;
        };
        if Arc::strong_count(&cached.index) != 1 {
            return false;
        }
        state.cached.take();
        true
    }
}

impl CachedIndex {
    fn new(index: Arc<Index>) -> Self {
        Self { index, used: Instant::now() }
    }
}

impl IndexReader {
    /// Locates a transaction by its first signature.
    pub(crate) fn transaction(&self, signature: &Signature) -> Result<Option<TxSpan>> {
        let Some(value) =
            self.snapshot.get(&self.index.transactions, prefix(signature.as_array()))?
        else {
            return Ok(None);
        };
        let value = fixed::<TX_SPAN_BYTES>(&value, "invalid transaction span value")?;
        Ok(Some(TxSpan {
            blockstore: Span::from_value(&value[..SPAN_BYTES])?,
            execution: Span::from_value(&value[SPAN_BYTES..])?,
        }))
    }

    /// Locates a block boundary by slot.
    pub(crate) fn block(&self, slot: Slot) -> Result<Option<Span>> {
        self.snapshot
            .get(&self.index.blocks, slot.to_be_bytes())?
            .map(|value| Span::from_value(&value))
            .transpose()
    }

    /// Scans a bounded slot range newest first.
    pub(crate) fn blocks(&self, slots: RangeInclusive<Slot>) -> BlockIter {
        let start = slots.start().to_be_bytes();
        let end = slots.end().to_be_bytes();
        BlockIter {
            inner: self.snapshot.range(&self.index.blocks, start..=end).rev(),
        }
    }

    /// Returns execution spans that mention `pubkey`, newest first.
    pub(crate) fn accounts(&self, pubkey: &Pubkey, before: Option<Span>) -> AccountIter {
        let prefix = prefix(pubkey.as_array());
        let inner = match before {
            Some(span) => self.snapshot.range(
                &self.index.accounts,
                prefixed_range(prefix, ..span.key_bytes()),
            ),
            None => self.snapshot.prefix(&self.index.accounts, prefix),
        };
        AccountIter { inner: inner.rev() }
    }
}

impl IndexWriter {
    /// Indexes a block boundary by slot.
    pub(crate) fn insert_block(&mut self, slot: Slot, span: Span) {
        self.batch.insert(&self.index.blocks, slot.to_be_bytes(), span.value_bytes());
    }

    /// Indexes a transaction and its execution details.
    pub(crate) fn insert_transaction(&mut self, signature: &Signature, span: TxSpan) {
        let mut value = [0; TX_SPAN_BYTES];
        value[..SPAN_BYTES].copy_from_slice(&span.blockstore.value_bytes());
        value[SPAN_BYTES..].copy_from_slice(&span.execution.value_bytes());
        self.batch.insert(
            &self.index.transactions,
            prefix(signature.as_array()),
            value,
        );
    }

    /// Adds account-to-execution entries for all static transaction accounts.
    pub(crate) fn insert_accounts(&mut self, accounts: &[Pubkey], span: Span) {
        for account in accounts {
            let mut key = [0; ACCOUNT_KEY_BYTES];
            key[..PREFIX_BYTES].copy_from_slice(&prefix(account.as_array()));
            key[PREFIX_BYTES..].copy_from_slice(&span.key_bytes());
            self.batch.insert(&self.index.accounts, key, []);
        }
    }

    /// Publishes the pending atomic batch at the requested durability.
    pub(crate) fn persist(&mut self, durability: Durability) -> Result<()> {
        let batch = std::mem::replace(&mut self.batch, self.index.db.batch());
        if batch.is_empty() {
            if durability.requires_sync() {
                self.index.db.persist(PersistMode::SyncData)?;
            }
            return Ok(());
        }
        let mode = match durability {
            Durability::Buffer => PersistMode::Buffer,
            Durability::SyncData => PersistMode::SyncData,
        };
        batch.durability(Some(mode)).commit().map_err(Into::into)
    }
}

/// Returns the compact prefix used by signature and account indexes.
fn prefix<const N: usize>(bytes: &[u8; N]) -> Prefix {
    let mut prefix = [0; PREFIX_BYTES];
    prefix.copy_from_slice(&bytes[..PREFIX_BYTES]);
    prefix
}

/// Converts persisted bytes to a fixed-width array or reports corruption.
fn fixed<const N: usize>(bytes: &[u8], error: &'static str) -> Result<[u8; N]> {
    bytes.try_into().map_err(|_| LedgerError::Corruption(error))
}
