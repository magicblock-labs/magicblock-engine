//! LMDB index schema and codecs for ledger blockstore entries.

use std::{array, borrow::Cow, fs, path::Path};

use bytemuck::{Pod, Zeroable};
use heed::{
    BoxedError, BytesDecode, BytesEncode, Database, DatabaseFlags, Env, EnvFlags, EnvOpenOptions,
    IntegerComparator, RoIter, byteorder::LittleEndian,
    iteration_method::MoveOnCurrentKeyDuplicates, types::U64,
};
use nucleus::{
    Slot,
    heed::{DatabaseIndex, OptRoTxn, OptRwTxn},
};
use solana_pubkey::Pubkey;
use solana_signature::Signature;

use crate::schema::Offset;

/// Index directory below each superblock directory.
const INDEX_SUBDIR: &str = "index";
/// Maximum LMDB map size for ledger indexes.
#[cfg(feature = "testkit")]
const INDEX_MAP_SIZE: usize = 32 * nucleus::MB;
#[cfg(not(feature = "testkit"))]
const INDEX_MAP_SIZE: usize = 16 * nucleus::GB;
/// Number of LMDB named databases in the ledger index.
const INDEX_DBS: u32 = 3;
/// Transaction signature index name.
const TRANSACTIONS_INDEX: &str = "transactions";
/// Slot-to-offset index name.
const SLOTS_INDEX: &str = "slots";
/// Account-to-offset index name.
const ACCOUNTS_INDEX: &str = "accounts";
/// Bytes kept from wide keys in compact index keys.
const KEY_BYTES: usize = 16;

/// Result type returned by heed codec hooks.
type CodecResult<T> = Result<T, BoxedError>;
/// Little-endian u64 codec used by LMDB keys and values.
type U64Le = U64<LittleEndian>;
/// Little-endian slot key with integer ordering.
type SlotKey = U64<LittleEndian>;
/// Truncated transaction signature key.
///
/// The 16-byte prefix is used as an index tag, not as a collision-proof
/// identity. Collision probability is negligible for the ledger's expected
/// scale, so the index accepts that risk to keep keys compact.
#[derive(Pod, Zeroable, Clone, Copy)]
#[repr(C)]
pub(crate) struct SignatureKey([u8; KEY_BYTES]);

/// Truncated account pubkey key.
///
/// See `SignatureKey`; account history uses the same compact prefix tag.
#[derive(Pod, Zeroable, Clone, Copy)]
#[repr(C)]
pub(crate) struct AccountKey([u8; KEY_BYTES]);

/// Packed span inside a ledger data file.
///
/// The high 39 bits store the byte offset. The low 25 bits store the encoded
/// entry size. Transaction entries are bounded by Solana's transaction-size
/// limit, and execution details are expected to stay at a few dozen KiB before
/// compression, so spans stay well below the 32 MiB encoded-size cap.
#[derive(Zeroable, Pod, Clone, Copy, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(C)]
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
}

/// Pair of blockstore-file and execution-file spans for a transaction.
#[derive(Zeroable, Pod, Clone, Copy)]
#[repr(C)]
pub(crate) struct TxSpan {
    /// Span of the raw transaction entry in the blockstore.
    pub(crate) blockstore: Span,
    /// Span of the execution details.
    pub(crate) execution: Span,
}

/// Account-index span codec.
pub(crate) enum AccountSpan {}

/// Duplicate account-index iterator over execution spans.
pub(crate) type AccountIter<'a> = RoIter<'a, AccountKey, AccountSpan, MoveOnCurrentKeyDuplicates>;

/// LMDB databases used to locate ledger data.
pub(crate) struct Index {
    /// Owning LMDB environment.
    env: Env,
    /// Transaction signature to transaction/execution spans.
    transactions: Database<SignatureKey, TxSpan>,
    /// Slot to blockstore-file span.
    blocks: Database<SlotKey, Span, IntegerComparator>,
    /// Account key to execution-details span.
    accounts: Database<AccountKey, AccountSpan>,
}

impl Index {
    /// Opens or creates the index directory and databases.
    pub(crate) fn new(path: &Path) -> heed::Result<Self> {
        let path = path.join(INDEX_SUBDIR);
        fs::create_dir_all(&path)?;
        // SAFETY: this process owns the index directory for the lifetime of
        // the database, so the backing files are not mutated behind LMDB's back.
        let env = unsafe {
            EnvOpenOptions::new()
                .max_dbs(INDEX_DBS)
                .map_size(INDEX_MAP_SIZE)
                .flags(EnvFlags::WRITE_MAP)
                .flags(EnvFlags::NO_SYNC)
                .open(path)?
        };

        let mut txn = env.write_txn()?;
        let transactions =
            env.database_options().name(TRANSACTIONS_INDEX).types().create(&mut txn)?;
        let blocks = env
            .database_options()
            .name(SLOTS_INDEX)
            .key_comparator()
            .types()
            .create(&mut txn)?;
        let accounts = env
            .database_options()
            .name(ACCOUNTS_INDEX)
            .flags(DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED | DatabaseFlags::REVERSE_DUP)
            .types()
            .create(&mut txn)?;
        txn.commit()?;
        Ok(Self {
            env,
            transactions,
            blocks,
            accounts,
        })
    }

    /// Indexes a block boundary by slot.
    pub(crate) fn insert_block(
        &self,
        txn: OptRwTxn<'_, '_>,
        slot: &Slot,
        span: &Span,
    ) -> heed::Result<()> {
        let txn = DatabaseIndex::write_txn(self, txn)?;
        self.blocks.put(txn, slot, span)
    }

    /// Indexes a transaction and its execution details.
    pub(crate) fn insert_transaction(
        &self,
        txn: OptRwTxn<'_, '_>,
        signature: &Signature,
        span: &TxSpan,
    ) -> heed::Result<()> {
        let txn = DatabaseIndex::write_txn(self, txn)?;
        self.transactions.put(txn, signature, span)
    }

    /// Adds account-to-execution entries for all static transaction accounts.
    pub(crate) fn insert_accounts(
        &self,
        txn: OptRwTxn<'_, '_>,
        accounts: &[Pubkey],
        span: &Span,
    ) -> heed::Result<()> {
        let txn = DatabaseIndex::write_txn(self, txn)?;
        for account in accounts {
            self.accounts.put(txn, account, span)?;
        }
        Ok(())
    }

    /// Locates a transaction by its first signature.
    pub(crate) fn transaction(
        &self,
        signature: &Signature,
        txn: OptRoTxn<'_, '_>,
    ) -> heed::Result<Option<TxSpan>> {
        let txn = DatabaseIndex::read_txn(self, txn)?;
        self.transactions.get(txn, signature)
    }

    /// Locates a block boundary by slot.
    pub(crate) fn block(&self, slot: &Slot, txn: OptRoTxn<'_, '_>) -> heed::Result<Option<Span>> {
        let txn = DatabaseIndex::read_txn(self, txn)?;
        self.blocks.get(txn, slot)
    }

    /// Returns execution spans that mention `pubkey`.
    pub(crate) fn accounts<'t, 'e>(
        &'e self,
        pubkey: &Pubkey,
        txn: OptRoTxn<'t, 'e>,
    ) -> heed::Result<Option<AccountIter<'t>>> {
        let txn = DatabaseIndex::read_txn(self, txn)?;
        self.accounts.get_duplicates(txn, pubkey)
    }
}

unsafe impl DatabaseIndex for Index {
    fn env(&self) -> &Env {
        &self.env
    }
}

impl<S: AsRef<Signature>> From<S> for SignatureKey {
    fn from(signature: S) -> Self {
        Self(array::from_fn(|i| signature.as_ref().as_array()[i]))
    }
}

impl<'a> BytesEncode<'a> for SignatureKey {
    type EItem = Signature;

    fn bytes_encode(item: &'a Self::EItem) -> CodecResult<Cow<'a, [u8]>> {
        Ok(item.as_array()[..KEY_BYTES].into())
    }
}

impl<'a> BytesEncode<'a> for AccountKey {
    type EItem = Pubkey;

    fn bytes_encode(item: &'a Self::EItem) -> CodecResult<Cow<'a, [u8]>> {
        Ok(item.as_array()[..KEY_BYTES].into())
    }
}

impl<'a> BytesDecode<'a> for SignatureKey {
    type DItem = &'a Self;

    fn bytes_decode(bytes: &'a [u8]) -> CodecResult<Self::DItem> {
        bytemuck::try_from_bytes(bytes).map_err(Into::into)
    }
}

impl<'a> BytesDecode<'a> for AccountKey {
    type DItem = &'a Self;

    fn bytes_decode(bytes: &'a [u8]) -> CodecResult<Self::DItem> {
        bytemuck::try_from_bytes(bytes).map_err(Into::into)
    }
}

impl<'a> BytesEncode<'a> for Span {
    type EItem = Self;

    fn bytes_encode(item: &'a Self::EItem) -> CodecResult<Cow<'a, [u8]>> {
        U64Le::bytes_encode(&item.0)
    }
}

impl<'a> BytesDecode<'a> for Span {
    type DItem = Self;

    fn bytes_decode(bytes: &'a [u8]) -> CodecResult<Self::DItem> {
        U64Le::bytes_decode(bytes).map(Self)
    }
}

impl<'a> BytesEncode<'a> for AccountSpan {
    type EItem = Span;

    fn bytes_encode(item: &'a Self::EItem) -> CodecResult<Cow<'a, [u8]>> {
        // `REVERSE_DUP` compares fixed-size values from the end, which makes
        // little-endian u64 values sort numerically ascending. Store the
        // inverted span so higher execution offsets are returned first.
        Ok((!item.0).to_le_bytes().to_vec().into())
    }
}

impl<'a> BytesDecode<'a> for AccountSpan {
    type DItem = Span;

    fn bytes_decode(bytes: &'a [u8]) -> CodecResult<Self::DItem> {
        U64Le::bytes_decode(bytes).map(|span| Span(!span))
    }
}

impl<'a> BytesEncode<'a> for TxSpan {
    type EItem = Self;

    fn bytes_encode(item: &'a Self::EItem) -> CodecResult<Cow<'a, [u8]>> {
        Ok(bytemuck::bytes_of(item).into())
    }
}

impl<'a> BytesDecode<'a> for TxSpan {
    type DItem = Self;

    fn bytes_decode(bytes: &'a [u8]) -> CodecResult<Self::DItem> {
        bytemuck::try_pod_read_unaligned(bytes).map_err(Into::into)
    }
}
