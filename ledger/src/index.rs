//! Fjall index schema and codecs for ledger blockstore entries.

use std::{iter::Rev, ops::RangeInclusive, path::Path};

use fjall::{
    CompressionType, Database, Keyspace, KeyspaceCreateOptions, OwnedWriteBatch, PersistMode,
    config::{CompressionPolicy, FilterPolicy},
    util::prefixed_range,
};
use nucleus::{MB, Slot};
use solana_pubkey::Pubkey;
use solana_signature::Signature;

use crate::{
    Superblock,
    error::{LedgerError, Result},
    schema::Offset,
    storage::Durability,
};

/// Ledger-wide index directory.
const INDEX_SUBDIR: &str = "index";
/// Transaction signature key namespace.
const TRANSACTION: u8 = b't';
/// Slot-to-offset key namespace.
const BLOCK: u8 = b'b';
/// Account-to-offset key namespace.
const ACCOUNT: u8 = b'a';
/// Bytes kept from wide keys in compact index keys.
const PREFIX_BYTES: usize = 16;
/// Bytes occupied by one encoded span.
const SPAN_BYTES: usize = size_of::<u64>();
/// Bytes occupied by a transaction's two spans.
const TX_SPAN_BYTES: usize = 2 * SPAN_BYTES;
/// Bytes occupied by a namespaced truncated signature.
const TX_KEY_BYTES: usize = 1 + PREFIX_BYTES;
/// Bytes occupied by a namespaced slot.
const BLOCK_KEY_BYTES: usize = 1 + size_of::<Slot>();
/// Bytes occupied by a namespaced account prefix.
const ACCOUNT_PREFIX_BYTES: usize = 1 + PREFIX_BYTES;
/// Bytes occupied by an account prefix and ordered execution span.
const ACCOUNT_KEY_BYTES: usize = ACCOUNT_PREFIX_BYTES + SPAN_BYTES;
/// Ledger-wide block cache capacity.
const CACHE_SIZE: u64 = 64 * MB as u64;
/// Retained journal bytes before Fjall flushes keyspaces blocking reclamation.
const JOURNAL_SIZE: u64 = 256 * MB as u64;
/// Fjall maintenance workers assigned to the ledger index.
const WORKERS: usize = 2;

/// Truncated signature or account key.
///
/// The 16-byte prefix is a compact identity, not a collision-proof one. The
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
            let key = key.get(1..).ok_or(LedgerError::Corruption("invalid slot key"))?;
            let key = fixed(key, "invalid slot key")?;
            Ok((u64::from_be_bytes(key), Span::from_value(&value)?))
        })
    }
}

impl Iterator for AccountIter {
    type Item = Result<Span>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|entry| {
            let key = entry.key()?;
            key.get(ACCOUNT_PREFIX_BYTES..)
                .ok_or(LedgerError::Corruption("invalid account index key"))
                .and_then(Span::from_key)
        })
    }
}

/// Ledger-wide Fjall database partitioned by one keyspace per superblock.
#[derive(Clone)]
pub(crate) struct Index {
    db: Database,
}

/// Atomic index mutations accumulated for one published ledger boundary.
pub(crate) struct IndexWriter {
    /// Mutations pending publication at the next boundary.
    batch: OwnedWriteBatch,
    /// Active superblock keyspace.
    keyspace: Keyspace,
    /// Ledger-wide database receiving the pending mutations.
    db: Database,
}

/// Read access to one retained superblock's append-only index.
///
/// Point lookups use Fjall's latest visible sequence number. Range and prefix
/// calls already return iterators carrying their own snapshot-tracker nonce, so
/// retaining a database-wide snapshot here would add tracking work without
/// strengthening these reads.
pub(crate) struct IndexReader<'a>(&'a Keyspace);

impl Index {
    /// Opens or creates the ledger-wide index.
    pub(crate) fn new(path: &Path) -> Result<Self> {
        let db = Database::builder(path.join(INDEX_SUBDIR))
            .cache_size(CACHE_SIZE)
            .max_journaling_size(JOURNAL_SIZE)
            .worker_threads(WORKERS)
            .manual_journal_persist(true)
            .journal_compression(CompressionType::None)
            .open()?;
        Ok(Self { db })
    }

    /// Opens or creates one superblock keyspace.
    pub(crate) fn keyspace(&self, id: u64) -> Result<Keyspace> {
        let options = KeyspaceCreateOptions::default()
            .data_block_compression_policy(CompressionPolicy::disabled())
            .index_block_compression_policy(CompressionPolicy::disabled())
            .filter_policy(FilterPolicy::disabled());
        self.db.keyspace(&Superblock::name(id), || options).map_err(Into::into)
    }

    /// Destroys a truncated superblock keyspace.
    pub(crate) fn delete(&self, keyspace: Keyspace) -> Result<()> {
        self.db.delete_keyspace(keyspace).map_err(Into::into)
    }

    /// Creates the single writer for a superblock keyspace.
    pub(crate) fn writer(&self, keyspace: &Keyspace) -> IndexWriter {
        let batch = self.db.batch();
        IndexWriter {
            db: self.db.clone(),
            keyspace: keyspace.clone(),
            batch,
        }
    }
}

impl<'a> IndexReader<'a> {
    /// Borrows a retained superblock keyspace for point and iterator reads.
    pub(crate) fn new(keyspace: &'a Keyspace) -> Self {
        Self(keyspace)
    }

    /// Locates a transaction by its first signature.
    pub(crate) fn transaction(&self, signature: &Signature) -> Result<Option<TxSpan>> {
        let Some(value) = self.0.get(transaction_key(signature))? else {
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
        self.0.get(block_key(slot))?.map(|value| Span::from_value(&value)).transpose()
    }

    /// Scans a bounded slot range newest first.
    pub(crate) fn blocks(&self, slots: RangeInclusive<Slot>) -> BlockIter {
        let start = block_key(*slots.start());
        let end = block_key(*slots.end());
        BlockIter {
            inner: self.0.range(start..=end).rev(),
        }
    }

    /// Returns execution spans that mention `pubkey`, newest first.
    pub(crate) fn accounts(&self, pubkey: &Pubkey, before: Option<Span>) -> AccountIter {
        let prefix = account_prefix(pubkey);
        let inner = match before {
            Some(span) => self.0.range(prefixed_range(prefix, ..span.key_bytes())),
            None => self.0.prefix(prefix),
        };
        AccountIter { inner: inner.rev() }
    }
}

impl IndexWriter {
    /// Indexes a block boundary by slot.
    pub(crate) fn insert_block(&mut self, slot: Slot, span: Span) {
        self.batch.insert(&self.keyspace, block_key(slot), span.value_bytes());
    }

    /// Indexes a transaction and its execution details.
    pub(crate) fn insert_transaction(&mut self, signature: &Signature, span: TxSpan) {
        let mut value = [0; TX_SPAN_BYTES];
        value[..SPAN_BYTES].copy_from_slice(&span.blockstore.value_bytes());
        value[SPAN_BYTES..].copy_from_slice(&span.execution.value_bytes());
        self.batch.insert(&self.keyspace, transaction_key(signature), value);
    }

    /// Adds account-to-execution entries for all static transaction accounts.
    pub(crate) fn insert_accounts(&mut self, accounts: &[Pubkey], span: Span) {
        for account in accounts {
            self.batch.insert(&self.keyspace, account_key(account, span), []);
        }
    }

    /// Publishes the pending atomic batch at the requested durability.
    pub(crate) fn persist(&mut self, durability: Durability) -> Result<()> {
        let batch = std::mem::replace(&mut self.batch, self.db.batch());
        if batch.is_empty() {
            if durability.requires_sync() {
                self.db.persist(PersistMode::SyncData)?;
            }
            return Ok(());
        }
        let mode = match durability {
            Durability::Buffer => PersistMode::Buffer,
            Durability::SyncData => PersistMode::SyncData,
        };
        batch.durability(Some(mode)).commit().map_err(Into::into)
    }

    /// Queues the immutable keyspace's active memtable for background flushing.
    pub(crate) fn rotate_memtable(&self) -> Result<()> {
        self.keyspace.rotate_memtable()?;
        Ok(())
    }
}

/// Returns the namespaced transaction key.
fn transaction_key(signature: &Signature) -> [u8; TX_KEY_BYTES] {
    let mut key = [0; TX_KEY_BYTES];
    key[0] = TRANSACTION;
    key[1..].copy_from_slice(&prefix(signature.as_array()));
    key
}

/// Returns the namespaced, numerically ordered block key.
fn block_key(slot: Slot) -> [u8; BLOCK_KEY_BYTES] {
    let mut key = [0; BLOCK_KEY_BYTES];
    key[0] = BLOCK;
    key[1..].copy_from_slice(&slot.to_be_bytes());
    key
}

/// Returns the namespaced prefix shared by one account's entries.
fn account_prefix(pubkey: &Pubkey) -> [u8; ACCOUNT_PREFIX_BYTES] {
    let mut key = [0; ACCOUNT_PREFIX_BYTES];
    key[0] = ACCOUNT;
    key[1..].copy_from_slice(&prefix(pubkey.as_array()));
    key
}

/// Returns the namespaced account entry key ordered by execution span.
fn account_key(pubkey: &Pubkey, span: Span) -> [u8; ACCOUNT_KEY_BYTES] {
    let mut key = [0; ACCOUNT_KEY_BYTES];
    key[..ACCOUNT_PREFIX_BYTES].copy_from_slice(&account_prefix(pubkey));
    key[ACCOUNT_PREFIX_BYTES..].copy_from_slice(&span.key_bytes());
    key
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
