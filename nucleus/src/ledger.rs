//! Ledger block-boundary schema shared by storage-adjacent crates.

use solana_hash::Hash;
use wincode::{SchemaRead, SchemaWrite};

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
pub struct SuperblockSeal {
    /// Id of the superblock this seal closes.
    pub id: u64,
    /// Checksum of accountsdb at the moment the superblock was sealed.
    pub checksum: u64,
    /// Total committed transactions represented by the sealed accountsdb snapshot.
    pub transactions: u64,
}
