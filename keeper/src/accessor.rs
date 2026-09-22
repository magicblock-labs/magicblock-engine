//! Namespaced keeper access APIs.

use std::{ops::Deref, path::PathBuf, sync::Arc};

use accountsdb::{AccountEntry, AccountsDB, AccountsDBError};
use ledger::{
    LedgerRequestError,
    request::*,
    schema::{Event, Signed, SuperblockSeal, TransactionEntry, signature_prefix},
};
use nucleus::{
    Slot,
    ledger::{Block, BlockstorePosition},
    tls::{EncodedMessage, TlsManager},
};
use solana_account::{AccountSharedData, ReadableAccount};
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use solana_sdk_ids::sysvar;
use solana_signature::Signature;
use solana_svm::{
    transaction_execution_result::ExecutedTransaction,
    transaction_processing_result::TransactionProcessingResult,
};
use solana_sysvar::{
    clock::Clock,
    slot_hashes::{SlotHashes, SysvarId},
};
use solana_transaction_error::{TransactionError, TransactionResult};
use tokio::sync::mpsc::Receiver;

use crate::{
    AccountLease, ExecutionRecord, FullTransaction, Keeper, ResolvedTransaction,
    error::Result,
    subscriptions::TransactionLogs,
    util::{execution_commit, request, transaction_status},
};

/// Account operations namespace.
pub struct AccountsAccessor<'a> {
    pub(crate) keeper: &'a Keeper,
}

impl<'a> AccountsAccessor<'a> {
    /// Waits for exclusive mutation ownership of `pubkey`.
    pub async fn lock(&self, pubkey: Pubkey) -> AccountLease {
        self.keeper.caches.accounts.lock(pubkey).await
    }

    /// Returns recent transaction signatures that mention the account.
    pub async fn signatures(
        &self,
        params: AccountSignaturesParams,
    ) -> Result<Vec<AccountSignature>> {
        Ok(request(self.keeper, params, ReadRequest::AccountSignatures).await??)
    }

    /// Subscribes to updates for one account pubkey.
    pub fn subscribe(&self, account: Pubkey) -> Receiver<AccountSharedData> {
        self.keeper.subscriptions.accounts.subscribe(account)
    }

    /// Subscribes to account updates for accounts owned by `program`.
    pub fn subscribe_program(&self, program: Pubkey) -> Receiver<AccountEntry> {
        self.keeper.subscriptions.programs.subscribe(program)
    }

    /// Subscribes as the sole receiver of account pubkeys evicted from the recency cache.
    ///
    /// Returns an error if the process-lifetime eviction receiver was already registered.
    pub fn subscribe_evictions(&self) -> Result<Receiver<Pubkey>> {
        self.keeper.caches.accounts.evictions.subscribe()
    }

    /// Subscribes to completed accountsdb snapshot archives.
    pub fn subscribe_snapshots(&self) -> Receiver<PathBuf> {
        self.keeper.subscriptions.snapshots.subscribe(())
    }

    /// Updates durable `SlotHashes` and `Clock` sysvar accounts from `block`.
    ///
    /// If either sysvar account is absent, no account updates are stored.
    pub fn update_sysvars(&self, block: Block) -> Result<()> {
        let loader = self.loader();

        let Some(mut hacc) = loader.read(&SlotHashes::id(), Clone::clone)? else {
            return Ok(());
        };
        let mut hashes: SlotHashes = hacc.deserialize_data().map_err(AccountsDBError::from)?;
        hashes.add(block.slot, block.hash);
        hacc.serialize_data(&hashes).map_err(AccountsDBError::from)?;
        let Some(mut cacc) = loader.read(&Clock::id(), Clone::clone)? else {
            return Ok(());
        };
        let clock = self.keeper.clock(block);
        cacc.serialize_data(&clock).map_err(AccountsDBError::from)?;
        drop(loader);
        self.store(&[(SlotHashes::id(), hacc), (Clock::id(), cacc)]).map_err(Into::into)
    }
}

impl Deref for AccountsAccessor<'_> {
    type Target = AccountsDB;

    fn deref(&self) -> &Self::Target {
        &self.keeper.accountsdb
    }
}

/// Transaction operations namespace.
pub struct TransactionsAccessor<'a> {
    pub(crate) keeper: &'a Keeper,
}

impl<'a> TransactionsAccessor<'a> {
    /// Loads the full retained transaction for `signature`.
    pub async fn get(&self, signature: Signature) -> Result<Option<TransactionResponse>> {
        Ok(request(self.keeper, signature, ReadRequest::Transaction).await??)
    }

    /// Loads the retained execution status for `signature`.
    pub async fn status(&self, signature: Signature) -> Result<Option<TransactionStatus>> {
        if let Some(status) = self.keeper.caches.signatures.get(&signature_prefix(&signature)) {
            return Ok(status);
        }
        Ok(request(self.keeper, signature, ReadRequest::TransactionStatus).await??)
    }

    /// Subscribes as the sole receiver of all processed transactions, including failures.
    ///
    /// Returns an error if the process-lifetime transaction receiver was already registered.
    pub fn subscribe_processed(&self) -> Result<Receiver<FullTransaction>> {
        self.keeper.subscriptions.transactions.subscribe()
    }

    /// Subscribes to one execution result, including an already retained result.
    /// Admission rejections are request-specific and do not notify observers.
    pub async fn subscribe_signature(
        &self,
        signature: Signature,
    ) -> Result<oneshot::Receiver<TransactionStatus>> {
        let subscriptions = &self.keeper.subscriptions.signatures;
        let rx = subscriptions.subscribe(signature);
        // Register before reading: commit caches its result before fanout, so
        // racing publication is observed through either the channel or status.
        if let Some(status) = self.status(signature).await? {
            subscriptions.send(&signature, &status);
        }
        Ok(rx)
    }

    /// Subscribes to log batches mentioning `account`.
    pub async fn subscribe_logs(&self, account: Pubkey) -> Receiver<Arc<TransactionLogs>> {
        self.keeper.subscriptions.logs.subscribe(account)
    }

    /// Subscribes as the sole receiver of encoded service messages.
    ///
    /// Returns an error if the process-lifetime service receiver was already registered.
    pub fn subscribe_service_messages(&self) -> Result<Receiver<EncodedMessage>> {
        self.keeper.subscriptions.services.subscribe()
    }

    /// Appends transaction bytes to the ledger, deduplicating by the first 16
    /// signature bytes. Distinct signatures with the same live prefix are
    /// treated as already processed.
    ///
    /// The inner result reports admission; the outer result reports infrastructure
    /// failure. Rejections never publish or cache an execution result. A rejected
    /// signature remains reserved until normal expiry. Execution details are
    /// appended later by `commit_execution`.
    pub async fn append(&self, transaction: &ResolvedTransaction) -> Result<TransactionResult<()>> {
        let caches = &self.keeper.caches;
        let slot = caches.blocks.latest.load().slot + 1;
        let signature = transaction.signatures()[0];
        let key = signature_prefix(&signature);
        if !caches.signatures.push(key, None, slot) {
            return Ok(Err(TransactionError::AlreadyProcessed));
        }
        if !self.keeper.blocks().is_valid(transaction.recent_blockhash()) {
            return Ok(Err(TransactionError::BlockhashNotFound));
        }
        let event = Event::Transaction(TransactionEntry {
            signature,
            payload: transaction.inner_data().clone(),
        });
        self.keeper.ledger.appender.send_async(event).await?;
        Ok(Ok(()))
    }

    /// Commits execution metadata and publishes resulting account changes.
    /// Returns the committed result for the caller to complete its request.
    pub fn commit_execution(&self, mut txn: FullTransaction) -> Result<TransactionResult<()>> {
        let subs = &self.keeper.subscriptions;
        let commit = execution_commit(&mut txn);
        self.keeper.ledger.appender.send(commit.event)?;

        if let Some(execution) = self.commit_state_transitions(&txn.execution.result)? {
            let accounts = &execution.loaded_transaction.accounts;
            let mut logs = None;
            for (pubkey, acc) in accounts {
                if subs.logs.contains(pubkey) {
                    let logs = logs.get_or_insert_with(|| {
                        Arc::new(TransactionLogs {
                            signature: commit.signature,
                            result: commit.status.result.clone(),
                            logs: Arc::clone(&commit.logs),
                        })
                    });
                    subs.logs.send(pubkey, logs);
                }
                if !acc.dirty() {
                    continue;
                }
                subs.accounts.send(pubkey, acc);
                if subs.programs.contains(acc.owner()) {
                    let account = &(*pubkey, acc.clone());
                    subs.programs.send(acc.owner(), account);
                }
            }
            while let Some(msg) = TlsManager::dequeue() {
                subs.services.blocking_send(|| msg);
            }
        }
        subs.transactions.blocking_send(|| {
            // Subscriber queues outlive the execution barrier. Materialize
            // borrowed accounts before handing off, while their mmap images
            // are still protected by execution's account ownership.
            if let Ok(execution) = &mut txn.execution.result {
                for (_, account) in &mut execution.loaded_transaction.accounts {
                    if matches!(account.cow(), solana_account::CoWAccount::Borrowed(_)) {
                        *account = account.clone();
                    }
                }
            }
            txn
        });
        // Clear TLS unconditionally so unsent messages cannot leak into the next transaction.
        TlsManager::clear();
        // Cache before fanout so a subscriber that misses delivery can still
        // discover the committed result.
        let key = &signature_prefix(&commit.signature);
        self.keeper.caches.signatures.update(key, Some(commit.status.clone()));
        subs.signatures.send(&commit.signature, &commit.status);
        Ok(commit.status.result)
    }

    /// Commits replayed state and caches its re-executed terminal status.
    ///
    /// Replay does not append ledger records or publish live subscriptions.
    pub fn commit_replay(
        &self,
        transaction: &ResolvedTransaction,
        execution: &ExecutionRecord,
    ) -> Result<()> {
        self.commit_state_transitions(&execution.result)?;
        let signature = transaction.signatures()[0];
        let status = transaction_status(&execution.result, execution.slot);
        let key = signature_prefix(&signature);
        self.keeper.caches.signatures.push(key, Some(status), execution.slot);
        Ok(())
    }

    /// Commits one accepted transaction to accountsdb, writing dirty accounts
    /// only for successful execution and returning it for downstream fanout.
    fn commit_state_transitions<'t>(
        &self,
        result: &'t TransactionProcessingResult,
    ) -> Result<Option<&'t ExecutedTransaction>> {
        let execution = result.as_ref().ok().filter(|e| e.was_successful());
        let accounts = execution
            .into_iter()
            .flat_map(|execution| execution.loaded_transaction.accounts.iter())
            .filter(|(id, a)| a.dirty() && !sysvar::instructions::check_id(id));
        self.keeper.accountsdb.commit(accounts)?;
        Ok(execution.map(|execution| &**execution))
    }
}

/// Block operations namespace.
pub struct BlocksAccessor<'a> {
    pub(crate) keeper: &'a Keeper,
}

impl<'a> BlocksAccessor<'a> {
    /// Loads a retained block at the requested detail level.
    pub async fn get(&self, params: BlockParams) -> Result<Option<BlockResponse>> {
        Ok(request(self.keeper, params, ReadRequest::Block).await??)
    }

    /// Returns the latest block boundary known to keeper.
    pub fn latest(&self) -> Block {
        **self.keeper.caches.blocks.latest.load()
    }

    /// Returns the slot currently being built (one past the latest block).
    pub fn current_slot(&self) -> Slot {
        self.keeper.caches.blocks.latest.load().slot + 1
    }

    /// Returns whether `hash` is in the recent block hash cache (still valid).
    pub fn is_valid(&self, hash: &Hash) -> bool {
        self.keeper.caches.blocks.history.contains(hash)
    }

    /// Subscribes to newly committed slots.
    pub fn subscribe(&self) -> Receiver<Block> {
        self.keeper.subscriptions.blocks.subscribe(())
    }

    /// Publishes a completed block and advances block-derived account state.
    ///
    /// Replay skips the ledger append because the block is already stored.
    pub fn append(&self, block: Signed<Block>, replay: bool) -> Result<()> {
        let event = Event::Block(block);
        if !replay {
            self.keeper.ledger.appender.send(event)?;
            self.keeper.subscriptions.blocks.send(&(), &block);
        }
        self.keeper.caches.blocks.push(*block);
        self.keeper.accounts().update_sysvars(*block)?;
        self.keeper.accounts().set_slot(block.slot)?;
        Ok(())
    }
}

/// Superblock operations namespace
pub struct SuperblockAccessor<'a> {
    pub(crate) keeper: &'a Keeper,
}

impl SuperblockAccessor<'_> {
    /// Reconstructed accountsdb seal state, without a producer signature.
    pub fn sealed(&self) -> SuperblockSeal {
        SuperblockSeal {
            id: self.keeper.accountsdb.superblock(),
            checksum: self.keeper.accountsdb.checksum(),
            transactions: self.keeper.accountsdb.transactions(),
        }
    }

    /// Returns the ledger root containing retained superblock directories.
    pub fn directory(&self) -> &PathBuf {
        &self.keeper.ledger.directory
    }

    /// Follower's current durable blockstore position, reported to the leader at handshake.
    pub fn position(&self) -> BlockstorePosition {
        self.keeper.ledger.position()
    }

    /// Enqueues a seal onto the append stream and returns its completion signal.
    ///
    /// The signal resolves after the appender durably seals the current
    /// superblock, adopts the seal's transaction count, and rotates to its successor.
    pub fn append(&self, seal: Signed<SuperblockSeal>) -> Result<oneshot::Receiver<()>> {
        let (response, completion) = oneshot::channel();
        let event = Event::Superblock { seal, response };
        self.keeper.ledger.appender.send(event)?;
        Ok(completion)
    }

    /// Blocks until every queued append event has been flushed and made durable.
    pub fn sync(&self, is_final: bool) -> Result<()> {
        let (response, ack) = oneshot::channel();
        let event = Event::Sync { response, is_final };
        self.keeper.ledger.appender.send(event)?;
        ack.recv().map_err(LedgerRequestError::from).map_err(Into::into)
    }
}
