//! Broadcast channels for live read-side notifications.

use std::{hash::Hash, path::PathBuf, sync::Arc, time::Duration};

use accountsdb::AccountEntry;
use ahash::RandomState;
use derive_more::Deref;
use ledger::{request::TransactionStatus, schema::Block};
use nucleus::{
    shutdown::{Service, ShutdownHandle, ShutdownManager, ShutdownReason},
    tls::EncodedMessage,
};
use scc::HashMap;
use solana_account::AccountSharedData;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction_error::TransactionResult;
use tokio::{
    sync::broadcast::{self, Receiver, Sender},
    time::{MissedTickBehavior, interval},
};

use crate::{
    FullTransaction,
    metrics::{self, Operation},
};

/// Composite log notification broadcast to log subscribers
#[derive(Clone)]
pub struct TransactionLogs {
    /// First transaction signature.
    pub signature: Signature,
    /// Runtime transaction result (carries the error on failure).
    pub result: TransactionResult<()>,
    /// Log lines emitted during execution.
    pub logs: Arc<Vec<String>>,
}

/// Live notification channels owned by keeper.
pub(crate) struct Subscriptions {
    /// Account updates keyed by account pubkey.
    pub(crate) accounts: Subscribers<Pubkey, AccountSharedData>,
    /// Program account updates keyed by owner pubkey;
    pub(crate) programs: Subscribers<Pubkey, AccountEntry>,
    /// Signature status updates keyed by transaction signature.
    pub(crate) signatures: Subscribers<Signature, TransactionStatus>,
    /// Log broadcasts keyed by mentioned program or account pubkey.
    pub(crate) logs: Subscribers<Pubkey, Arc<TransactionLogs>>,
    /// Broadcast channel for newly committed slots.
    pub(crate) blocks: Sender<Block>,
    /// Broadcast channel for all committed transactions.
    pub(crate) transactions: Sender<Arc<FullTransaction>>,
    /// Accountsdb snapshot archive completions, sent after compression finishes.
    pub(crate) snapshots: Sender<PathBuf>,
    /// Encoded service messages emitted during successful transaction execution.
    pub(crate) services: Sender<EncodedMessage>,
}

/// Lazily-created per-key broadcast channel map.
#[derive(Deref)]
pub(crate) struct Subscribers<K, V>(HashMap<K, Sender<V>, RandomState>);

impl<K, V> Default for Subscribers<K, V> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<K: Eq + Hash, V: Clone> Subscribers<K, V> {
    /// Returns a receiver for `key`, creating its broadcast channel on demand.
    pub(crate) async fn subscribe(&self, key: K, cap: usize) -> Receiver<V> {
        self.entry_async(key)
            .await
            .or_insert_with(|| broadcast::channel(cap).0)
            .subscribe()
    }

    /// Broadcasts `value` to every subscriber on `key`, dropping the channel
    /// afterwards if the last receiver is gone or `oneshot` marks `key` as
    /// terminal (a final update, e.g. a settled signature status).
    #[inline]
    pub(crate) fn send(&self, key: &K, value: &V, oneshot: bool) {
        let sender = |_: &K, tx: &Sender<V>| tx.send(value.clone()).is_ok();
        let success = self.read_sync(key, sender).unwrap_or(true);
        if !success || oneshot {
            self.remove_sync(key);
        }
    }

    /// Returns whether `key` currently has a subscription channel.
    #[inline]
    pub(crate) fn contains(&self, key: &K) -> bool {
        if self.is_empty() {
            return false;
        }
        self.contains_sync(key)
    }
}

impl Subscriptions {
    /// Builds subscription channels and starts cleanup for idle keyed entries.
    pub(crate) fn new(shutdown: &mut ShutdownManager) -> Arc<Self> {
        let (transactions, _) = broadcast::channel(1024);
        let (blocks, _) = broadcast::channel(32);
        let (snapshots, _) = broadcast::channel(4);
        let (services, _) = broadcast::channel(64);
        let subs = Arc::new(Self {
            accounts: Default::default(),
            programs: Default::default(),
            signatures: Default::default(),
            logs: Default::default(),
            blocks,
            transactions,
            snapshots,
            services,
        });
        let shutdown = shutdown.handle(Service::SubscriptionsCleanup);
        tokio::spawn(cleanup(subs.clone(), shutdown));
        subs
    }
}

/// Drops keyed broadcast channels after their last receiver is gone.
async fn cleanup(subscriptions: Arc<Subscriptions>, mut shutdown: ShutdownHandle) {
    let mut ticker = interval(Duration::from_secs(60));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.signalled() => break,
            _ = ticker.tick() => {
                let _timer = metrics::time(Operation::Cleanup);
                subscriptions.accounts.0.retain_async(|_, s| s.receiver_count() != 0).await;
                subscriptions.programs.0.retain_async(|_, s| s.receiver_count() != 0).await;
                subscriptions.signatures.0.retain_async(|_, s| s.receiver_count() != 0).await;
                subscriptions.logs.0.retain_async(|_, s| s.receiver_count() != 0).await;
            }
        }
    }
    shutdown.terminate(ShutdownReason::Signalled);
}
