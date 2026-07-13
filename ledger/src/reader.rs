//! Ledger read-side worker implementation.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read},
    mem,
    ops::Range,
    os::unix::fs::FileExt,
    sync::{Arc, atomic::Ordering::Acquire},
};

use agave_transaction_view::transaction_view::SanitizedTransactionView;
use bitcode::Buffer;
use flume::Receiver;
use nucleus::{
    Slot,
    heed::OptRoTxn,
    shutdown::{ShutdownHandle, ShutdownReason},
};
use solana_signature::Signature;
use tracing::error;
use wincode::{Error, io::Cursor};
use zstd::bulk::Decompressor;

use crate::{
    Ledger, LedgerError, Result, Superblock,
    index::{AccountIter, Span},
    metrics::{self, Operation},
    request::{
        AccountSignature, AccountSignaturesParams, AccountSignaturesPayload,
        AccountSignaturesReadResult, BlockDetails, BlockParams, BlockPayload, BlockReadResult,
        BlockResponse, BlockWithSignatures, BlockWithTransactions, FullBlockInfo, ReadRequest,
        ReplayParams, ReplayPayload, TransactionPayload, TransactionReadResult,
        TransactionResponse, TransactionStatus, TransactionStatusPayload,
        TransactionStatusReadResult,
    },
    schema::{
        Block, BlockstoreEntry, Execution, ExecutionHeader, MAX_EXECUTION_DETAILS_SIZE,
        OwnedBlockstoreEntry, blockstore,
    },
};

/// Stateful ledger reader with reusable decoder buffers.
pub(crate) struct LedgerReader {
    /// Request stream shared by callers through `LedgerHandle`.
    rx: Receiver<ReadRequest>,
    /// Shared top-level ledger state.
    ledger: Arc<Ledger>,
    /// Reusable zstd decompressor.
    decompressor: Decompressor<'static>,
    /// Scratch buffers reused across requests.
    buffers: ReadBuffers,
}

impl LedgerReader {
    /// Creates a reader over `ledger`.
    pub(crate) fn new(ledger: Arc<Ledger>, rx: Receiver<ReadRequest>) -> Result<Self> {
        let mut buffers = ReadBuffers::default();
        buffers.details.resize(MAX_EXECUTION_DETAILS_SIZE, 0);
        Ok(Self {
            rx,
            ledger,
            decompressor: Decompressor::new()?,
            buffers,
        })
    }

    /// Serves requests until every sender has been dropped.
    pub(crate) fn run(mut self, mut shutdown: ShutdownHandle) {
        while let Ok(request) = self.rx.recv() {
            match request {
                ReadRequest::Shutdown => break,
                ReadRequest::Transaction(r) => {
                    let _timer = metrics::time(Operation::ReadTransaction);
                    let result = self.transaction(&r);
                    let _ = r.response.send(result);
                }
                ReadRequest::TransactionStatus(r) => {
                    let _timer = metrics::time(Operation::ReadTransactionStatus);
                    let result = self.transaction_status(&r);
                    let _ = r.response.send(result);
                }
                ReadRequest::AccountSignatures(r) => {
                    let _timer = metrics::time(Operation::ReadAccountSignatures);
                    let result = self.account_signatures(&r);
                    let _ = r.response.send(result);
                }
                ReadRequest::Block(r) => {
                    let _timer = metrics::time(Operation::ReadBlock);
                    let result = self.block(&r);
                    let _ = r.response.send(result);
                }
                ReadRequest::BlockRange(r) => {
                    let _timer = metrics::time(Operation::ReadBlockRange);
                    let result = self.blocks(r.params);
                    let _ = r.response.send(result);
                }
                ReadRequest::Replay(r) => {
                    let _timer = metrics::time(Operation::Replay);
                    let result = self.replay(&r);
                    let _ = r.response.send(result);
                }
            }
        }
        // Release ledger ownership before the manager can reopen it.
        drop(self);
        shutdown.terminate(ShutdownReason::Signalled);
    }

    /// Reads a full transaction and its execution details.
    fn transaction(&mut self, request: &TransactionPayload) -> TransactionReadResult {
        for superblock in self.ledger.clone().iter() {
            if request.cancelled() {
                return Ok(None);
            }
            let Some(spans) = superblock.index.transaction(&request.params, &mut None)? else {
                continue;
            };
            return Ok(Some(TransactionResponse {
                transaction: self.transaction_entry(&superblock, spans.blockstore)?,
                execution: self.execution(&superblock, spans.execution)?,
            }));
        }
        Ok(None)
    }

    /// Reads the status header for a transaction.
    fn transaction_status(
        &mut self,
        request: &TransactionStatusPayload,
    ) -> TransactionStatusReadResult {
        for superblock in self.ledger.clone().iter() {
            if request.cancelled() {
                return Ok(None);
            }
            let Some(spans) = superblock.index.transaction(&request.params, &mut None)? else {
                continue;
            };
            let header = self.header(&superblock, spans.execution)?;
            return Ok(Some(TransactionStatus {
                result: header.result,
                slot: header.slot,
            }));
        }
        Ok(None)
    }

    /// Reads recent signatures that mention an account.
    fn account_signatures(
        &mut self,
        request: &AccountSignaturesPayload,
    ) -> AccountSignaturesReadResult {
        let mut signatures = Vec::new();
        let AccountSignaturesParams { pubkey, limit, mut before, until } = request.params;
        let mut blocktimes = HashMap::<Slot, i64>::new();
        for superblock in self.ledger.clone().iter() {
            if request.cancelled() {
                return Ok(signatures);
            }
            let mut txn = None;
            let Some(iter) = superblock.index.accounts(&pubkey, &mut txn)? else {
                continue;
            };
            // SAFETY: `iter` borrows the read transaction stored in `txn`.
            // `txn` was populated by `accounts`, remains in this stack frame,
            // is not replaced or dropped while `iter` is used, and every later
            // index read only reuses that already-open read transaction.
            let iter = unsafe { mem::transmute::<AccountIter<'_>, AccountIter<'_>>(iter) };

            let mut upper = None;
            if let Some(signature) = &before {
                // Skip newest-first segments until `before`, then include every older segment.
                let Some(cutoff) = superblock.index.transaction(signature, &mut txn)? else {
                    continue;
                };
                upper.replace(cutoff.execution);
                before.take();
            }

            for result in iter {
                let span = result?.1;
                if let Some(s) = upper
                    && span >= s
                {
                    continue;
                };
                let header = self.header(&superblock, span)?;
                let sig: Signature = header.signature;
                if let Some(signature) = until
                    && sig == signature
                {
                    return Ok(signatures);
                }
                // Several matching signatures can share a slot, so cache block timestamps per slot.
                let blocktime = match blocktimes.get(&header.slot) {
                    Some(time) => *time,
                    None => {
                        let time = self.blocktime(&superblock, header.slot, &mut txn)?;
                        blocktimes.insert(header.slot, time);
                        time
                    }
                };
                signatures.push(AccountSignature {
                    signature: sig,
                    result: header.result,
                    slot: header.slot,
                    blocktime,
                });
                if signatures.len() >= limit {
                    return Ok(signatures);
                }
            }
        }
        Ok(signatures)
    }

    /// Reads a block with the requested transaction detail level.
    fn block(&mut self, request: &BlockPayload) -> BlockReadResult {
        let slot = request.params.slot;
        if !self.ledger.meta.range.contains(&slot) {
            return Ok(None);
        }

        let mut position = None;
        for superblock in self.ledger.clone().iter() {
            if request.cancelled() {
                return Ok(None);
            }
            if !superblock.meta.range.contains(&slot) {
                continue;
            }
            let Some(span) = superblock.index.block(&slot, &mut None)? else {
                return Ok(None);
            };
            position.replace((superblock, span));
            break;
        }
        let Some((superblock, span)) = position else { return Ok(None) };
        self.populate_block_response(&superblock, request, span).map(Some)
    }

    /// Reads block boundaries for a slot range in ascending slot order.
    fn blocks(&mut self, range: Range<Slot>) -> Result<Vec<Block>> {
        let mut blocks = Vec::with_capacity(range.clone().count());
        let mut range = range.into_iter().rev().peekable();

        for superblock in self.ledger.clone().iter() {
            let mut txn = None;
            let start = superblock.meta.range.start.load(Acquire);
            while let Some(&slot) = range.peek() {
                // Slots descend and superblocks go newest to oldest, so a slot
                // below this segment's start belongs to an older superblock;
                // leave it in the iterator rather than consuming it here.
                if slot < start {
                    break;
                }
                range.next();
                let Some(span) = superblock.index.block(&slot, &mut txn)? else {
                    continue;
                };
                if let BlockstoreEntry::Block(b) = self.blockstore_entry(&superblock, span)? {
                    blocks.push(b);
                }
            }
        }
        blocks.reverse();
        Ok(blocks)
    }

    fn replay(&mut self, request: &ReplayPayload) -> Result<()> {
        let ReplayParams { superblock, tx } = &request.params;
        for superblock in self.ledger.iter_after(*superblock) {
            let limit = superblock.meta.cursors.blockstore.load(Acquire);
            let mut reader = BufReader::new((&superblock.blockstore).take(limit));
            loop {
                if request.cancelled() || tx.is_closed() {
                    return Ok(());
                }
                if reader.fill_buf()?.is_empty() {
                    break;
                }
                let entry = blockstore::decode(&mut reader).map_err(Into::<Error>::into)?;
                if tx.blocking_send(entry).is_err() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Reads and decodes a raw transaction entry.
    fn transaction_entry(&mut self, superblock: &Superblock, span: Span) -> Result<Vec<u8>> {
        let entry = self.blockstore_entry(superblock, span)?;
        let BlockstoreEntry::Transaction(transaction) = entry else {
            error!(
                superblock = superblock.id,
                "ledger index points to invalid transaction entry"
            );
            return Err(LedgerError::Corruption(
                "index points to invalid transaction entry",
            ));
        };
        Ok(transaction)
    }

    /// Reads and decodes a complete execution entry.
    fn execution(&mut self, superblock: &Superblock, span: Span) -> Result<Execution> {
        self.buffers.read_executions(superblock, span)?;
        let header = self.decode_header()?;
        let header_size = wincode::serialized_size(&header).map_err(Into::<Error>::into)? as usize;

        let compressed = &self.buffers.executions[header_size..];
        let size = self
            .decompressor
            .decompress_to_buffer(compressed, self.buffers.details.as_mut_slice())?;
        let details = self.buffers.decoder.decode(&self.buffers.details[..size])?;
        Ok(Execution { header, details })
    }

    /// Reads and decodes one blockstore entry at an indexed span.
    fn blockstore_entry(
        &mut self,
        superblock: &Superblock,
        span: Span,
    ) -> Result<OwnedBlockstoreEntry> {
        self.buffers.read_blockstore(superblock, span)?;
        let bytes = self.buffers.blockstore.as_slice();
        let entry = blockstore::decode(bytes).map_err(Into::<Error>::into)?;
        Ok(entry)
    }

    /// Builds the block response requested by the caller.
    fn populate_block_response(
        &mut self,
        superblock: &Superblock,
        request: &BlockPayload,
        span: Span,
    ) -> Result<BlockResponse> {
        let BlockParams { slot, details } = request.params;
        let BlockstoreEntry::Block(block) = self.blockstore_entry(superblock, span)? else {
            error!(
                superblock = superblock.id,
                slot, "ledger index points to invalid block entry"
            );
            return Err(LedgerError::Corruption(
                "index points to invalid block entry",
            ));
        };

        if matches!(details, BlockDetails::None) {
            return Ok(BlockResponse::Bare(block));
        }
        let mut txn = None;
        // Slots are contiguous, so the previous block boundary is always `slot - 1`.
        let start = match superblock.index.block(&(slot - 1), &mut txn)? {
            Some(previous) => previous.offset() + previous.size(),
            None => 0,
        };
        self.buffers.read_blockstore_range(superblock, start..span.offset())?;
        let blockstore = mem::take(&mut self.buffers.blockstore);
        let len = blockstore.len();
        let mut cursor = Cursor::new(blockstore);
        let result = (|| {
            let mut full = Vec::new();
            let mut transactions = Vec::new();
            let mut signatures = Vec::new();
            while cursor.position() < len && !request.cancelled() {
                let entry = blockstore::decode(&mut cursor).map_err(Into::<Error>::into)?;
                let BlockstoreEntry::Transaction(transaction) = entry else {
                    error!(
                        superblock = superblock.id,
                        slot, "ledger blockstore includes invalid block entries"
                    );
                    return Err(LedgerError::Corruption(
                        "blockstore includes invalid block entries",
                    ));
                };
                if matches!(details, BlockDetails::Transactions) {
                    transactions.push(transaction);
                    continue;
                }
                let signature = signature(&transaction)?;
                if matches!(details, BlockDetails::Signatures) {
                    signatures.push(signature);
                    continue;
                }
                let Some(spans) = superblock.index.transaction(&signature, &mut txn)? else {
                    continue;
                };
                let execution = self.execution(superblock, spans.execution)?;
                full.push(TransactionResponse { transaction, execution });
            }
            Ok((full, transactions, signatures))
        })();
        self.buffers.blockstore = cursor.into_inner();
        let (full, transactions, signatures) = result?;

        let response = match details {
            BlockDetails::Full => BlockResponse::Full(FullBlockInfo { block, transactions: full }),
            BlockDetails::Transactions => {
                BlockResponse::WithTransactions(BlockWithTransactions { block, transactions })
            }
            BlockDetails::Signatures => {
                BlockResponse::WithSignatures(BlockWithSignatures { block, signatures })
            }
            BlockDetails::None => BlockResponse::Bare(block),
        };
        Ok(response)
    }

    /// Reads the timestamp from a block boundary entry.
    fn blocktime(
        &mut self,
        superblock: &Superblock,
        slot: Slot,
        txn: OptRoTxn<'_, '_>,
    ) -> Result<i64> {
        let Some(span) = superblock.index.block(&slot, txn)? else {
            return Ok(0);
        };
        let entry = self.blockstore_entry(superblock, span)?;
        let BlockstoreEntry::Block(block) = entry else {
            return Ok(0);
        };
        Ok(block.time)
    }

    /// Reads only the fixed execution header.
    fn header(&mut self, superblock: &Superblock, span: Span) -> Result<ExecutionHeader> {
        self.buffers.read_executions(superblock, span)?;
        self.decode_header()
    }

    /// Decodes the execution header currently loaded in the executions buffer.
    fn decode_header(&self) -> Result<ExecutionHeader> {
        let header = wincode::deserialize(self.buffers.executions.as_slice())
            .map_err(Into::<Error>::into)?;
        Ok(header)
    }
}

/// Reusable read buffers owned by one reader task.
#[derive(Default)]
struct ReadBuffers {
    /// Blockstore-file scratch bytes.
    blockstore: Vec<u8>,
    /// Executions-file scratch bytes.
    executions: Vec<u8>,
    /// Bitcode decoder state for execution details.
    decoder: Buffer,
    /// Decompressed execution-detail payload.
    details: Vec<u8>,
}

impl ReadBuffers {
    /// Reads blockstore-file bytes at `span` into the reusable buffer.
    fn read_blockstore(&mut self, superblock: &Superblock, span: Span) -> Result<()> {
        self.blockstore.resize(span.size() as usize, 0);
        superblock.blockstore.read_exact_at(&mut self.blockstore, span.offset())?;
        Ok(())
    }

    /// Reads executions-file bytes at `span` into the reusable buffer.
    fn read_executions(&mut self, superblock: &Superblock, span: Span) -> Result<()> {
        self.executions.resize(span.size() as usize, 0);
        superblock.executions.read_exact_at(&mut self.executions, span.offset())?;
        Ok(())
    }

    /// Reads a byte range from the blockstore file into the reusable buffer.
    fn read_blockstore_range(&mut self, superblock: &Superblock, range: Range<u64>) -> Result<()> {
        self.blockstore.resize((range.end - range.start) as usize, 0);
        superblock.blockstore.read_exact_at(&mut self.blockstore, range.start)?;
        Ok(())
    }
}

fn signature(transaction: &[u8]) -> Result<Signature> {
    SanitizedTransactionView::try_new_sanitized(transaction, false)
        .map(|transaction| transaction.signatures()[0])
        .map_err(Into::into)
}

// SAFETY: `LedgerReader` is moved into one background thread and keeps its
// decoder and decompressor state on that thread for the reader lifetime.
unsafe impl Send for LedgerReader {}
