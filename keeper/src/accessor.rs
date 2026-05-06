//! Namespaced keeper access APIs.

use std::{ops::Deref, path::PathBuf, sync::Arc};

use accountsdb::{AccountEntry, AccountLoader, AccountsDB, AccountsDBError};
use ledger::{
    LedgerRequestError,
    request::*,
    schema::{Event, SuperblockSeal, TransactionEntry},
};
use nucleus::{
    Slot,
    ledger::{Block, BlockstorePosition},
    tls::{EncodedMessage, TlsManager},
};
use solana_account::{AccountSharedData, ReadableAccount};
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_svm::{
    transaction_execution_result::ExecutedTransaction,
    transaction_processing_result::TransactionProcessingResult,
};
use solana_sysvar::{
    clock::Clock,
    slot_hashes::{SlotHashes, SysvarId},
};
use tokio::sync::broadcast::Receiver;

use crate::{
    FullTransaction, Keeper, ResolvedTransaction,
    cache::{AccountCache, MissingAccount},
    error::Result,
    metrics,
    subscriptions::TransactionLogs,
    util::{execution_commit, request},
};

/// Account operations namespace.
pub struct AccountsAccessor<'a> {
    pub(crate) keeper: &'a Keeper,
}

/// Cursor over accounts missing from local storage.
///
/// Each item either gives the caller load ownership or waits for another
/// caller that already owns the load.
pub struct MissingAccounts<'a> {
    index: usize,
    accounts: &'a [Pubkey],
    loader: AccountLoader<'a>,
    cache: &'a Arc<AccountCache>,
}

impl<'a> Iterator for MissingAccounts<'a> {
    type Item = MissingAccount;
    /// Returns the next account that still needs resolution.
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let pubkey = self.accounts.get(self.index)?;
            self.index += 1;
            if self.loader.contains(pubkey).unwrap_or_default() {
                self.cache.promote(pubkey);
                continue;
            }
            return Some(self.cache.reserve(*pubkey));
        }
    }
}

impl<'a> AccountsAccessor<'a> {
    /// Loads the current account state for `pubkey`.
    pub fn get(&self, pubkey: &Pubkey) -> Result<Option<AccountSharedData>> {
        self.loader().load(pubkey).map_err(Into::into)
    }

    /// Coordinates loading of accounts missing from local storage.
    pub fn ensure(&'a self, accounts: &'a [Pubkey]) -> MissingAccounts<'a> {
        MissingAccounts {
            index: 0,
            accounts,
            loader: self.keeper.accountsdb.loader(),
            cache: &self.keeper.caches.accounts,
        }
    }

    /// Returns recent transaction signatures that mention the account.
    pub async fn signatures(
        &self,
        params: AccountSignaturesParams,
    ) -> Result<Vec<AccountSignature>> {
        Ok(request(self.keeper, params, ReadRequest::AccountSignatures).await??)
    }

    /// Subscribes to updates for one account pubkey.
    pub async fn subscribe(&self, account: Pubkey) -> Receiver<AccountSharedData> {
        self.keeper.subscriptions.accounts.subscribe(account, 8).await
    }

    /// Subscribes to account updates for accounts owned by `program`.
    pub async fn subscribe_program(&self, program: Pubkey) -> Receiver<AccountEntry> {
        self.keeper.subscriptions.programs.subscribe(program, 16).await
    }

    /// Subscribes to account pubkeys evicted from the recent-load cache.
    pub fn subscribe_evictions(&self) -> Receiver<Pubkey> {
        self.keeper.caches.accounts.evictions.subscribe()
    }

    /// Subscribes to completed accountsdb snapshot archives.
    pub fn subscribe_snapshots(&self) -> Receiver<PathBuf> {
        self.keeper.subscriptions.snapshots.subscribe()
    }

    /// Updates durable `SlotHashes` and `Clock` sysvar accounts from `block`.
    ///
    /// If either sysvar account is absent, no account updates are stored.
    pub fn update_sysvars(&self, block: Block) -> Result<()> {
        let loader = self.loader();

        let Some(mut hacc) = loader.load(&SlotHashes::id())? else {
            return Ok(());
        };
        let mut hashes: SlotHashes = hacc.deserialize_data().map_err(AccountsDBError::from)?;
        hashes.add(block.slot, block.hash);
        hacc.serialize_data(&hashes).map_err(AccountsDBError::from)?;
        let Some(mut cacc) = loader.load(&Clock::id())? else {
            return Ok(());
        };
        let mut clock: Clock = cacc.deserialize_data().map_err(AccountsDBError::from)?;
        clock.slot = block.slot;
        clock.unix_timestamp = block.time;
        cacc.serialize_data(&clock).map_err(AccountsDBError::from)?;
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
        if let Some(status) = self.keeper.caches.signatures.get(&signature) {
            return Ok(status);
        }
        Ok(request(self.keeper, signature, ReadRequest::TransactionStatus).await??)
    }

    /// Subscribes to all processed transactions, including failed executions.
    pub fn subscribe_processed(&self) -> Receiver<Arc<FullTransaction>> {
        self.keeper.subscriptions.transactions.subscribe()
    }

    /// Subscribes to status updates for one transaction signature.
    pub async fn subscribe_signature(&self, signature: Signature) -> Receiver<TransactionStatus> {
        self.keeper.subscriptions.signatures.subscribe(signature, 1).await
    }

    /// Subscribes to log batches mentioning `account`.
    pub async fn subscribe_logs(&self, account: Pubkey) -> Receiver<Arc<TransactionLogs>> {
        self.keeper.subscriptions.logs.subscribe(account, 8).await
    }

    /// Subscribes to encoded service messages emitted by successful transactions.
    pub fn subscribe_service_messages(&self) -> Receiver<EncodedMessage> {
        self.keeper.subscriptions.services.subscribe()
    }

    /// Appends transaction bytes to the ledger, deduplicating by signature.
    ///
    /// Returns `Ok(true)` when the transaction was appended, or `Ok(false)`
    /// without appending if its signature was already seen for this slot.
    /// Execution details are appended later by `commit_execution`.
    pub async fn append(&self, transaction: &ResolvedTransaction) -> Result<bool> {
        let caches = &self.keeper.caches;
        let slot = caches.blocks.latest.load().slot + 1;
        let signature = transaction.signatures()[0];
        if !caches.signatures.push(signature, None, slot) {
            return Ok(false);
        }
        if !self.keeper.blocks().is_valid(transaction.recent_blockhash()) {
            return Ok(false);
        }
        metrics::signature_entries(caches.signatures.len());
        let event = Event::Transaction(TransactionEntry {
            signature,
            payload: transaction.inner_data().clone(),
        });
        self.keeper.ledger.appender.send_async(event).await?;
        Ok(true)
    }

    /// Commits execution metadata and publishes resulting account changes.
    pub fn commit_execution(&self, mut txn: FullTransaction) -> Result<()> {
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
                    subs.logs.send(pubkey, logs, false);
                }
                if !acc.dirty() {
                    continue;
                }
                subs.accounts.send(pubkey, acc, false);
                if subs.programs.contains(acc.owner()) {
                    let account = &(*pubkey, acc.clone());
                    subs.programs.send(acc.owner(), account, false);
                }
            }
            if subs.services.receiver_count() != 0 {
                while let Some(msg) = TlsManager::dequeue() {
                    let _ = subs.services.send(msg);
                }
            }
        }
        if subs.transactions.receiver_count() != 0 {
            let _ = subs.transactions.send(Arc::new(txn));
        }
        // Clear TLS unconditionally so unsent messages cannot leak into the next transaction.
        TlsManager::clear();
        subs.signatures.send(&commit.signature, &commit.status, true);
        self.keeper.caches.signatures.update(&commit.signature, Some(commit.status));
        Ok(())
    }

    /// Writes the dirty accounts of a successful execution back to accountsdb,
    /// returning the execution (for downstream fanout) or `None` if it failed.
    pub fn commit_state_transitions<'t>(
        &self,
        result: &'t TransactionProcessingResult,
    ) -> Result<Option<&'t ExecutedTransaction>> {
        let Some(execution) = result.as_ref().ok().filter(|e| e.was_successful()) else {
            return Ok(None);
        };
        let accounts = &execution.loaded_transaction.accounts;
        self.keeper.accountsdb.store(accounts.iter().filter(|(_, a)| a.dirty()))?;
        Ok(Some(&**execution))
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
        self.keeper.subscriptions.blocks.subscribe()
    }

    /// Publishes a completed block and advances block-derived account state.
    ///
    /// Replay skips the ledger append because the block is already stored.
    pub fn append(&self, block: Block, replay: bool) -> Result<()> {
        let event = Event::Block(block);
        if !replay {
            self.keeper.ledger.appender.send(event)?;
            self.keeper.caches.blocks.push(block);
            if self.keeper.subscriptions.blocks.receiver_count() != 0 {
                let _ = self.keeper.subscriptions.blocks.send(block);
            }
        }
        self.keeper.accounts().update_sysvars(block)?;
        self.keeper.accounts().set_slot(block.slot)?;
        Ok(())
    }
}

/// Superblock operations namespace
pub struct SuperblockAccessor<'a> {
    pub(crate) keeper: &'a Keeper,
}

impl SuperblockAccessor<'_> {
    /// Id of the superblock accountsdb last sealed.
    pub fn sealed(&self) -> SuperblockSeal {
        SuperblockSeal {
            id: self.keeper.accountsdb.superblock(),
            checksum: self.keeper.accountsdb.checksum(),
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

    /// Enqueues a seal onto the append stream, sealing the current superblock and
    /// rotating to the next. Used by a follower applying a seal received from the leader.
    pub fn append(&self, seal: SuperblockSeal) -> Result<()> {
        let event = Event::Superblock(seal);
        self.keeper.ledger.appender.send(event)?;
        self.sync(false)
    }

    /// Blocks until every queued append event has been flushed and made durable.
    pub fn sync(&self, is_final: bool) -> Result<()> {
        let (response, ack) = oneshot::channel();
        let event = Event::Sync { response, is_final };
        self.keeper.ledger.appender.send(event)?;
        ack.recv().map_err(LedgerRequestError::from)?.map_err(Into::into)
    }
}
