//! Ledger wire, event, and on-disk blockstore formats.
//!
//! The blockstore is a wincode stream of raw transactions in execution order.
//! Block entries delimit the transactions that belong to each Solana-like
//! block, and a superblock seal terminates a truncatable group of blocks.
//! Execution details live in a separate file as a wincode header followed by
//! a zstd-compressed bitcode payload.

use std::sync::Arc;

use bitcode::{Decode, Encode};
use nucleus::Slot;
pub use nucleus::ledger::{Block, SuperblockSeal};

use solana_signature::Signature;
use solana_transaction_error::TransactionResult;
use wincode::{SchemaRead, SchemaWrite};

use crate::{error::Result, index::Span};

/// Byte offset into a ledger data file.
pub(crate) type Offset = u64;

/// Owned blockstore entry used when replaying persisted ledger data.
pub type OwnedBlockstoreEntry = BlockstoreEntry<Vec<u8>>;

/// Largest encoded ledger entry representable by an index span.
pub const MAX_ENTRY_SIZE: usize = Span::MAX_SIZE as usize;

/// Largest decoded execution-details payload retained by the ledger.
///
/// Compressed execution records are normally only a few KiB, so 4 MiB leaves
/// substantial headroom while bounding decompression and bitcode decoding.
pub(crate) const MAX_EXECUTION_DETAILS_SIZE: usize = 4 * nucleus::MB;

/// Codec for entries in the blockstore stream.
///
/// Owned payloads may allocate up to [`MAX_ENTRY_SIZE`], rather than wincode's
/// smaller default. Writers enforce the encoded-size bound before reaching this
/// codec.
pub mod blockstore {
    use super::{BlockstoreEntry, MAX_ENTRY_SIZE, OwnedBlockstoreEntry};
    use wincode::{
        ReadResult, WriteResult,
        config::Configuration,
        io::{Reader, Writer},
    };

    /// Decodes the next entry without consuming bytes belonging to a following entry.
    pub fn decode<'de>(src: impl Reader<'de>) -> ReadResult<OwnedBlockstoreEntry> {
        let config = Configuration::default().with_preallocation_size_limit::<MAX_ENTRY_SIZE>();
        wincode::config::deserialize_from(src, config)
    }

    /// Writes one entry without changing the default blockstore wire encoding.
    pub(crate) fn encode(dst: impl Writer, entry: &BlockstoreEntry<&[u8]>) -> WriteResult<()> {
        let config = Configuration::default().with_preallocation_size_limit::<MAX_ENTRY_SIZE>();
        wincode::config::serialize_into(dst, entry, config)
    }
}

/// Work item sent to the ledger appender.
// Keep execution inline to avoid one allocation per append event.
#[allow(clippy::large_enum_variant)]
pub enum Event {
    /// Transaction bytes accepted for later execution indexing.
    Transaction(TransactionEntry),
    /// Runtime execution metadata for a previously appended transaction.
    Execution(Execution),
    /// Block boundary marker and block hash.
    Block(Block),
    /// Seal the active superblock and rotate to a fresh directory.
    Superblock(SuperblockSeal),
    /// Install a snapshot seal and adopt its cumulative transaction count.
    Bootstrap(SuperblockSeal),
    /// Volatile accounts were discarded at `Slot` after upstream synchronization was lost.
    Reset(Slot),
    /// Flush pending appends and optionally stop the appender after acknowledging.
    Sync {
        /// Receives the durability result.
        response: oneshot::Sender<Result<()>>,
        /// Whether this is the terminal engine sync.
        is_final: bool,
    },
}

/// Raw transaction payload queued for blockstore append.
pub struct TransactionEntry {
    /// First transaction signature used for status and execution indexes.
    pub signature: Signature,
    /// Serialized sanitized transaction bytes shared with the appender.
    pub payload: Arc<Vec<u8>>,
}

/// One typed entry in the blockstore stream.
#[derive(SchemaRead, SchemaWrite)]
pub enum BlockstoreEntry<T> {
    /// Delimits the preceding transactions as one block and stores its hash.
    Block(Block),
    /// Serialized transaction bytes.
    Transaction(T),
    /// Superblock seal marker and snapshot checksum.
    Superblock(SuperblockSeal),
    /// Marks a volatile-state reset in the durable replay stream.
    Reset(Slot),
}

/// Fixed execution prefix stored before the compressed bitcode payload.
#[derive(SchemaRead, SchemaWrite)]
pub struct ExecutionHeader {
    /// Transaction signature.
    pub signature: Signature,
    /// Runtime transaction error, if execution failed.
    pub result: TransactionResult<()>,
    /// Slot where the transaction executed.
    pub slot: Slot,
}

/// Complete transaction execution metadata.
pub struct Execution {
    /// Fixed prefix used for cheap signature and error reads.
    pub header: ExecutionHeader,
    /// Execution details stored in the compressed payload.
    pub details: Option<ExecutionDetails>,
}

/// Transaction execution details stored after the execution header.
#[derive(Encode, Decode)]
pub struct ExecutionDetails {
    /// Fee charged for the transaction.
    pub fee: u64,
    /// Pre/post balance deltas encoded in a compact prototype layout.
    pub balances: Balances,
    /// Log messages emitted during execution.
    pub logs: Arc<Vec<String>>,
    /// Cross-program invocation trace.
    pub cpi: Option<Vec<Cpis>>,
    /// Compute units consumed.
    pub compute_units: u64,
    /// Program return data, if any.
    pub return_data: Option<ReturnData>,
}

/// One cross-program invocation group.
#[derive(Encode, Decode)]
pub struct Cpis(
    /// Inner instructions invoked by one transaction-level instruction.
    pub Vec<Instruction>,
);

/// Runtime instruction metadata.
#[derive(Encode, Decode)]
pub struct Instruction {
    /// Compiled Solana instruction.
    pub compiled: CompiledInstruction,
    /// Invocation stack height; transaction-level instructions start at 1.
    pub stack_height: u8,
}

/// Compact compiled instruction form.
#[derive(Encode, Decode)]
pub struct CompiledInstruction {
    /// Program account index.
    pub program_index: u8,
    /// Account indexes referenced by the instruction.
    pub accounts: Vec<u8>,
    /// Opaque instruction data.
    pub data: Vec<u8>,
}

/// Program return data.
#[derive(Encode, Decode)]
pub struct ReturnData {
    /// Program that produced the return data.
    pub program: [u8; 32],
    /// Opaque return bytes.
    pub data: Arc<Vec<u8>>,
}

/// Pre/post balance vectors.
#[derive(Encode, Decode)]
pub struct Balances {
    /// Balances before execution.
    pub pre: Vec<u64>,
    /// Balances after execution.
    pub post: Vec<u64>,
}
