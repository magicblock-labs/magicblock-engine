//! Ledger block-boundary schema shared by storage-adjacent crates.

use derive_more::Deref;
use solana_hash::Hash;
use solana_keypair::{Keypair, Signer};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use wincode::{Result, SchemaRead, SchemaWrite, config::DefaultConfig};

use crate::Slot;

/// File name of the archived accountsdb snapshot tarball inside a superblock directory.
pub const ACCOUNTSDB_SNAPSHOT_FILE: &str = "accountsdb.tar.zst";

/// A byte cursor into the ledger blockstore stream, used by the replication path
/// to mark how far a follower has consumed. Ordering is lexicographic over
/// `(superblock, offset)`, matching the on-disk append order across rotations.
#[derive(Clone, Copy, SchemaRead, SchemaWrite, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct BlockstorePosition {
    /// Superblock whose blockstore file the offset indexes into.
    pub superblock: u64,
    /// Byte offset of the write cursor within that superblock's blockstore file.
    pub offset: u64,
}

/// Block boundary entry stored after all transactions in the block.
#[derive(SchemaRead, SchemaWrite, Clone, Copy, Default, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Block {
    /// Slot that produced the block.
    pub slot: Slot,
    /// Block hash for `slot`.
    pub hash: Hash,
    /// Block timestamp in the producer's time base.
    pub time: i64,
    /// Hash of the preceding block.
    pub parent: Hash,
}

impl Block {
    /// Creates a block boundary whose hash-chain metadata is not yet known.
    pub fn new(slot: Slot, time: i64) -> Self {
        Self { slot, time, ..Default::default() }
    }
}

/// Superblock boundary entry stored at the end of the blockstore stream.
#[derive(SchemaRead, SchemaWrite, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct SuperblockSeal {
    /// Id of the superblock this seal closes.
    pub id: u64,
    /// Checksum of accountsdb at the moment the superblock was sealed.
    pub checksum: u64,
    /// Total committed transactions represented by the sealed accountsdb snapshot.
    pub transactions: u64,
}

/// Volatile-state reset stored at its ordered position in the ledger.
#[derive(SchemaRead, SchemaWrite, Clone, Debug, PartialEq, Eq)]
pub struct Reset {
    /// Slot at which volatile state is discarded.
    pub slot: Slot,
    /// Rent rate effective before subsequent transactions.
    pub rent: u64,
    /// Complete active feature set.
    pub features: Vec<Pubkey>,
}

/// Fresh persisted-state checksum at an ordered ledger boundary.
#[derive(SchemaRead, SchemaWrite, Clone, Copy, Debug, PartialEq, Eq, Deref)]
#[repr(transparent)]
pub struct Checkpoint(pub u64);

/// A ledger payload and its producer signature, preserved by storage and replay.
#[derive(SchemaRead, SchemaWrite, Clone, Copy, Debug, PartialEq, Eq, Deref)]
pub struct Signed<T> {
    /// Unsigned record encoded by its wincode schema.
    #[deref]
    pub payload: T,
    /// Signature over the domain followed by the encoded payload.
    pub signature: Signature,
}

const DOMAIN_SIZE: usize = 8;

/// Canonically serialized ledger payload with a distinct signing domain.
pub trait Record: SchemaWrite<DefaultConfig, Src = Self> {
    /// Domain prefix separating this payload from other signed records.
    const DOMAIN: &'static [u8; DOMAIN_SIZE];
}

impl Record for Block {
    const DOMAIN: &'static [u8; DOMAIN_SIZE] = b"MB:block";
}
impl Record for SuperblockSeal {
    const DOMAIN: &'static [u8; DOMAIN_SIZE] = b"MB:seal\0";
}
impl Record for Reset {
    const DOMAIN: &'static [u8; DOMAIN_SIZE] = b"MB:reset";
}
impl Record for Checkpoint {
    const DOMAIN: &'static [u8; DOMAIN_SIZE] = b"MB:check";
}

impl<T: Record> Signed<T> {
    /// Signs the domain-prefixed canonical payload. Encoding failure is a programming error.
    #[expect(
        clippy::expect_used,
        reason = "Engine-owned record encoding is an invariant; verification remains fallible"
    )]
    pub fn new(payload: T, signer: &Keypair) -> Self {
        let bytes = Self::message(&payload).expect("Engine-owned ledger records must be encodable");
        let signature = signer.sign_message(&bytes);
        Self { payload, signature }
    }

    /// Authenticates the canonical payload against the configured producer.
    pub fn verify(&self, authority: &Pubkey) -> bool {
        Self::message(&self.payload)
            .is_ok_and(|bytes| self.signature.verify(authority.as_ref(), &bytes))
    }

    fn message(payload: &T) -> Result<Vec<u8>> {
        let mut bytes = T::DOMAIN.to_vec();
        wincode::serialize_into(&mut bytes, payload)?;
        Ok(bytes)
    }
}
