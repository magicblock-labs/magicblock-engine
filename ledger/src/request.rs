//! Ledger read-side request and response types.

use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use flume::Sender as RequestSender;
use nucleus::Slot;
use oneshot::{Receiver, Sender};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction_error::TransactionResult;
use tokio::{sync::mpsc, time};

use crate::{
    Result,
    error::RequestResult,
    schema::{Block, Execution, OwnedBlockstoreEntry},
};

/// Result returned by a full transaction lookup.
pub(crate) type TransactionReadResult = Result<Option<TransactionResponse>>;
/// Result returned by a transaction-status lookup.
pub(crate) type TransactionStatusReadResult = Result<Option<TransactionStatus>>;
/// Result returned by an account-signature history lookup.
pub(crate) type AccountSignaturesReadResult = Result<Vec<AccountSignature>>;
/// Result returned by a block lookup.
pub(crate) type BlockReadResult = Result<Option<BlockResponse>>;

/// Payload for a transaction lookup request.
pub(crate) type TransactionPayload = RequestPayload<Signature, TransactionReadResult>;
/// Payload for a transaction-status lookup request.
pub(crate) type TransactionStatusPayload = RequestPayload<Signature, TransactionStatusReadResult>;
/// Payload for an account-signature history request.
pub(crate) type AccountSignaturesPayload =
    RequestPayload<AccountSignaturesParams, AccountSignaturesReadResult>;
/// Payload for a single-block lookup request.
pub(crate) type BlockPayload = RequestPayload<BlockParams, BlockReadResult>;
/// Payload for a contiguous block-range request.
pub(crate) type BlockRangePayload = RequestPayload<Range<Slot>, Result<Vec<Block>>>;
/// Payload for replaying owned blockstore entries after a sealed superblock.
pub(crate) type ReplayPayload = RequestPayload<ReplayParams, Result<()>>;

/// Read request queue consumed by reader workers.
pub type ReaderSender = RequestSender<ReadRequest>;

/// Handle for consuming a ledger replay stream.
pub struct ReplayHandle {
    /// Entries streamed from retained blockstore data in on-disk order.
    pub rx: mpsc::Receiver<OwnedBlockstoreEntry>,
    /// Completion result sent after the reader finishes streaming entries.
    pub response: RequestHandle<Result<()>>,
}

/// Signature summary returned for account history queries.
pub struct AccountSignature {
    /// Transaction signature.
    pub signature: Signature,
    /// Slot where the transaction executed.
    pub slot: Slot,
    /// Runtime transaction result.
    pub result: TransactionResult<()>,
    /// Block timestamp for `slot`, or `0` when unavailable.
    pub blocktime: i64,
}

/// Full transaction response with optional execution metadata.
pub struct TransactionResponse {
    /// Serialized transaction bytes from the blockstore file.
    pub transaction: Vec<u8>,
    /// Execution metadata when requested or available.
    pub execution: Execution,
}

/// Cheap transaction-status response.
#[derive(Clone)]
pub struct TransactionStatus {
    /// Runtime transaction result.
    pub result: TransactionResult<()>,
    /// Slot where the transaction executed.
    pub slot: Slot,
}

/// Account-signature query parameters.
pub struct AccountSignaturesParams {
    /// Account pubkey to search for.
    pub pubkey: Pubkey,
    /// Maximum number of signatures to return.
    pub limit: usize,
    /// Signature before which results should start.
    pub before: Option<Signature>,
    /// Signature at which results should stop.
    pub until: Option<Signature>,
}

/// Parameters for streaming retained blockstore entries into a replay consumer.
pub struct ReplayParams {
    /// Last sealed superblock already reflected in the consumer's state.
    pub superblock: u64,
    /// Channel receiving blockstore entries in on-disk order.
    pub tx: mpsc::Sender<OwnedBlockstoreEntry>,
}

/// Read request sent to a ledger reader service.
pub enum ReadRequest {
    /// Stop this reader after all requests queued before this marker.
    Shutdown,
    /// Full transaction lookup by signature.
    Transaction(TransactionPayload),
    /// Transaction status lookup by signature.
    TransactionStatus(TransactionStatusPayload),
    /// Account-signature history lookup.
    AccountSignatures(AccountSignaturesPayload),
    /// Block lookup by slot and detail level.
    Block(BlockPayload),
    /// Contiguous block boundary lookup.
    BlockRange(BlockRangePayload),
    /// Replay retained blockstore entries after an applied superblock seal.
    Replay(ReplayPayload),
}

/// Block lookup response at the requested detail level.
pub enum BlockResponse {
    /// Block boundary only.
    Bare(Block),
    /// Block boundary with transaction signatures.
    WithSignatures(BlockWithSignatures),
    /// Block boundary with serialized transactions.
    WithTransactions(BlockWithTransactions),
    /// Block boundary with serialized transactions and execution metadata.
    Full(FullBlockInfo),
}

impl BlockResponse {
    /// Returns the block boundary included in every response variant.
    pub fn block(&self) -> &Block {
        match self {
            Self::Bare(b) => b,
            Self::WithSignatures(b) => &b.block,
            Self::WithTransactions(b) => &b.block,
            Self::Full(b) => &b.block,
        }
    }
}

/// Full block response with execution metadata.
pub struct FullBlockInfo {
    /// Block boundary.
    pub block: Block,
    /// Transactions with matching execution metadata.
    pub transactions: Vec<TransactionResponse>,
}

/// Block response containing serialized transactions only.
pub struct BlockWithTransactions {
    /// Block boundary.
    pub block: Block,
    /// Serialized transactions in block order.
    pub transactions: Vec<Vec<u8>>,
}

/// Block response containing transaction signatures only.
pub struct BlockWithSignatures {
    /// Block boundary.
    pub block: Block,
    /// Transaction signatures in block order.
    pub signatures: Vec<Signature>,
}

/// Handle used by the request caller to await a reader response.
pub struct RequestHandle<R> {
    /// One-shot response receiver.
    rx: Receiver<R>,
    /// Pending flag guard cleared when the response is no longer needed.
    pending: PendingGuard,
}

impl<R> RequestHandle<R> {
    /// Wait up to one minute for a reader response.
    ///
    /// Dropping the handle before a response clears the request's pending flag
    /// so long-running readers can stop work that no caller will receive.
    pub async fn recv_timeout(self) -> RequestResult<R> {
        const ONE_MINUTE: Duration = Duration::from_secs(60);
        time::timeout(ONE_MINUTE, self.recv()).await?
    }

    /// Wait for a reader response without applying a timeout.
    pub async fn recv(mut self) -> RequestResult<R> {
        let result = self.rx.await?;
        self.pending.0.take();
        Ok(result)
    }
}

/// Clears the request's pending flag when dropped.
struct PendingGuard(Option<Arc<AtomicBool>>);

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(pending) = self.0.take() {
            pending.store(false, Ordering::Release);
        }
    }
}

/// Request payload with cancellation and oneshot response channel.
pub struct RequestPayload<P, R> {
    /// Request parameters.
    pub params: P,
    /// Response channel for the result.
    pub response: Sender<R>,
    /// Set while the caller is still waiting for the response.
    pub pending: Arc<AtomicBool>,
}

impl<P, R> RequestPayload<P, R> {
    /// Creates a request payload and the handle that receives its response.
    pub fn new(params: P) -> (Self, RequestHandle<R>) {
        let (response, rx) = oneshot::channel();
        let pending = Arc::new(AtomicBool::new(true));
        let payload = Self {
            params,
            response,
            pending: pending.clone(),
        };
        let handle = RequestHandle {
            rx,
            pending: PendingGuard(Some(pending)),
        };
        (payload, handle)
    }

    /// Returns whether the caller stopped waiting for this response.
    pub(crate) fn cancelled(&self) -> bool {
        !self.pending.load(Ordering::Acquire)
    }
}

/// Block query parameters.
pub struct BlockParams {
    /// Slot to look up.
    pub slot: Slot,
    /// Amount of transaction data to include in the response.
    pub details: BlockDetails,
}

/// Transaction detail level for a block lookup.
#[derive(Clone, Copy)]
pub enum BlockDetails {
    /// Include transactions and execution details.
    Full,
    /// Include transactions without execution details.
    Transactions,
    /// Include only transaction signatures.
    Signatures,
    /// Include only the block boundary entry.
    None,
}
