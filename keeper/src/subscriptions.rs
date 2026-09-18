//! Live read-side notification channels.

use std::{
    hash::Hash,
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use accountsdb::AccountEntry;
use ahash::RandomState;
use derive_more::Deref;
use ledger::{request::TransactionStatus, schema::Block};
use nucleus::{
    shutdown::{Service, ShutdownHandle, ShutdownManager, ShutdownReason},
    tls::EncodedMessage,
};
use scc::HashMap;
use smallvec::SmallVec;
use solana_account::AccountSharedData;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction_error::TransactionResult;
use tokio::{
    sync::mpsc::{self, error::TrySendError},
    time::{MissedTickBehavior, interval},
};

use crate::{
    FullTransaction,
    error::{KeeperError, Result},
    metrics::{self, Operation},
};

/// Keeps the usual single receiver inline.
type Senders<S> = SmallVec<[S; 1]>;

/// Keyed receivers with a constant-time, conservative empty check.
#[derive(Deref)]
pub(crate) struct Subscribers<K, S> {
    /// Nonempty sender lists protected by SCC's bucket locks.
    #[deref]
    entries: HashMap<K, Senders<S>, RandomState>,
    /// Registered keys, including closed receivers until pruning. Detached lists
    /// remain counted until removal finishes, so zero never hides a live key.
    keys: AtomicUsize,
}

impl<K, S> Default for Subscribers<K, S> {
    /// Starts empty without allocating buckets.
    fn default() -> Self {
        Self {
            entries: Default::default(),
            keys: AtomicUsize::new(0),
        }
    }
}

impl<K: Eq + Hash, S> Subscribers<K, S> {
    /// Skips map access when no registered key remains.
    fn is_empty(&self) -> bool {
        self.keys.load(Ordering::Acquire) == 0
    }

    /// Registers under the bucket lock without waiting for channel capacity.
    fn insert(&self, key: K, sender: S) {
        let mut entry = self.entry_sync(key).or_default();
        if entry.is_empty() {
            self.keys.fetch_add(1, Ordering::Release);
        }
        entry.push(sender);
    }

    /// Prunes one key and reports whether it still has registered senders.
    fn retain(&self, key: &K, keep: impl FnMut(&S) -> bool) -> bool {
        if self.is_empty() {
            return false;
        }
        let mut present = false;
        self.remove_if_sync(key, |senders| {
            present = self.prune(senders, keep);
            !present
        });
        present
    }

    /// Prunes all keys, yielding between contended buckets.
    async fn cleanup(&self, mut keep: impl FnMut(&S) -> bool) {
        if !self.is_empty() {
            self.retain_async(|_, senders| self.prune(senders, &mut keep)).await;
        }
    }

    /// Called under the bucket lock; the caller must remove an emptied key
    /// before unlocking, so cleanup revisits cannot decrement it twice.
    fn prune(&self, senders: &mut Senders<S>, mut keep: impl FnMut(&S) -> bool) -> bool {
        senders.retain(|sender| keep(sender));
        if senders.is_empty() {
            self.keys.fetch_sub(1, Ordering::Release);
            return false;
        }
        true
    }

    /// Detaches one key for delivery without holding a map guard.
    fn remove(&self, key: &K) -> Option<Senders<S>> {
        if self.is_empty() {
            return None;
        }
        let (_, senders) = self.remove_sync(key)?;
        // A concurrent registration counts its own key; subtract only ours.
        self.keys.fetch_sub(1, Ordering::Release);
        Some(senders)
    }
}

/// Stable metric identity for a subscription stream.
#[derive(Clone, Copy)]
pub(crate) enum Subscription {
    Accounts,
    Programs,
    Logs,
    Blocks,
    Transactions,
    Snapshots,
    Services,
    Evictions,
}

impl Subscription {
    /// Stable label used by subscription metrics and errors.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Accounts => "accounts",
            Self::Programs => "programs",
            Self::Logs => "logs",
            Self::Blocks => "blocks",
            Self::Transactions => "transactions",
            Self::Snapshots => "snapshots",
            Self::Services => "services",
            Self::Evictions => "evictions",
        }
    }
}

/// Live notification channels owned by keeper.
pub(crate) struct Subscriptions {
    /// Account updates keyed by account pubkey.
    pub(crate) accounts: Multicast<Pubkey, AccountSharedData>,
    /// Program account updates keyed by owner pubkey.
    pub(crate) programs: Multicast<Pubkey, AccountEntry>,
    /// Signature status updates keyed by transaction signature.
    pub(crate) signatures: Signatures,
    /// Log notifications keyed by mentioned program or account pubkey.
    pub(crate) logs: Multicast<Pubkey, Arc<TransactionLogs>>,
    /// Newly committed blocks.
    pub(crate) blocks: Multicast<(), Block>,
    /// All committed transactions for the sole stream consumer.
    pub(crate) transactions: Unicast<FullTransaction>,
    /// Accountsdb snapshot archive completions.
    pub(crate) snapshots: Multicast<(), PathBuf>,
    /// Encoded service messages for the sole stream consumer.
    pub(crate) services: Unicast<EncodedMessage>,
}

impl Subscriptions {
    /// Builds subscription channels and starts cleanup for idle keyed entries.
    pub(crate) fn new(shutdown: &mut ShutdownManager) -> Arc<Self> {
        let subscriptions = Arc::new(Self {
            accounts: Multicast::new(8, Subscription::Accounts),
            programs: Multicast::new(16, Subscription::Programs),
            signatures: Default::default(),
            logs: Multicast::new(8, Subscription::Logs),
            blocks: Multicast::new(32, Subscription::Blocks),
            transactions: Unicast::new(1024, Subscription::Transactions),
            snapshots: Multicast::new(4, Subscription::Snapshots),
            services: Unicast::new(64, Subscription::Services),
        });
        let shutdown = shutdown.handle(Service::SubscriptionsCleanup);
        tokio::spawn(cleanup(subscriptions.clone(), shutdown));
        subscriptions
    }

    /// Reclaims abandoned keyed subscriptions without affecting unicast ownership.
    async fn cleanup(&self) {
        self.accounts.cleanup().await;
        self.programs.cleanup().await;
        self.signatures.cleanup().await;
        self.logs.cleanup().await;
        self.blocks.cleanup().await;
        self.snapshots.cleanup().await;
    }
}

/// Composite log notification sent to log subscribers.
#[derive(Clone)]
pub struct TransactionLogs {
    /// First transaction signature.
    pub signature: Signature,
    /// Runtime transaction result (carries the error on failure).
    pub result: TransactionResult<()>,
    /// Log lines emitted during execution.
    pub logs: Arc<Vec<String>>,
}

/// One process-lifetime bounded receiver.
pub(crate) struct Unicast<V> {
    /// Set once; receiver closure never permits re-registration.
    sender: OnceLock<mpsc::Sender<V>>,
    /// Queue bound before producers must wait.
    capacity: usize,
    /// Stream identity for registration errors.
    subscription: Subscription,
}

impl<V> Unicast<V> {
    /// Configures the stream without allocating a channel.
    pub(crate) const fn new(capacity: usize, subscription: Subscription) -> Self {
        Self {
            sender: OnceLock::new(),
            capacity,
            subscription,
        }
    }

    /// Creates the process-lifetime receiver, rejecting every later subscriber.
    pub(crate) fn subscribe(&self) -> Result<mpsc::Receiver<V>> {
        let (tx, rx) = mpsc::channel(self.capacity);
        self.sender
            .set(tx)
            .map_err(|_| KeeperError::SubscriptionRegistered(self.subscription.label()))?;
        Ok(rx)
    }

    /// Sends asynchronously, waiting until the receiver has capacity.
    pub(crate) async fn send(&self, value: V) {
        let Some(sender) = self.sender.get() else {
            return;
        };
        let _ = sender.send(value).await;
    }

    /// Prepares and sends a value, waiting for queue capacity.
    ///
    /// Skips preparation if no sender exists or it is observed closed.
    /// The receiver may close after this check.
    pub(crate) fn blocking_send(&self, prepare: impl FnOnce() -> V) {
        if let Some(sender) = self.sender.get()
            && !sender.is_closed()
        {
            let _ = sender.blocking_send(prepare());
        }
    }
}

/// Persistent per-key fanout over one bounded queue per receiver.
#[derive(Deref)]
pub(crate) struct Multicast<K, V> {
    /// Per-key receivers sharing the empty-path check.
    #[deref]
    senders: Subscribers<K, mpsc::Sender<V>>,
    /// Per-receiver queue bound; overflow disconnects that receiver.
    capacity: usize,
    /// Stream identity for slow-consumer metrics.
    subscription: Subscription,
}

impl<K: Eq + Hash, V> Multicast<K, V> {
    /// Configures queue bounds without registering receivers.
    pub(crate) fn new(capacity: usize, subscription: Subscription) -> Self {
        Self {
            senders: Default::default(),
            capacity,
            subscription,
        }
    }

    /// Adds a receiver for `key` with its own bounded queue.
    pub(crate) fn subscribe(&self, key: K) -> mpsc::Receiver<V> {
        let (tx, rx) = mpsc::channel(self.capacity);
        self.insert(key, tx);
        rx
    }

    /// Returns whether `key` has any live receivers.
    pub(crate) fn contains(&self, key: &K) -> bool {
        self.retain(key, |sender| !sender.is_closed())
    }

    /// Drops closed receivers while preserving every live queue.
    async fn cleanup(&self) {
        self.senders.cleanup(|sender| !sender.is_closed()).await;
    }

    /// Fans out without blocking, disconnecting receivers whose queues are full.
    pub(crate) fn send(&self, key: &K, value: &V)
    where
        V: Clone,
    {
        self.retain(key, |sender| match sender.try_send(value.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                metrics::slow_consumer_disconnect(self.subscription);
                false
            }
            Err(TrySendError::Closed(_)) => false,
        });
    }
}

/// Terminal execution-result fanout keyed by signature.
#[derive(Default)]
pub(crate) struct Signatures {
    /// One terminal channel per observer, grouped by signature.
    senders: Subscribers<Signature, oneshot::Sender<TransactionStatus>>,
}

impl Signatures {
    /// Registers before the caller checks retained status; publication caches
    /// status before fanout, closing the registration/publication race.
    pub(crate) fn subscribe(&self, signature: Signature) -> oneshot::Receiver<TransactionStatus> {
        let (tx, rx) = oneshot::channel();
        self.senders.insert(signature, tx);
        rx
    }

    /// Drops abandoned senders and keys with no remaining observers.
    pub(crate) async fn cleanup(&self) {
        self.senders.cleanup(|sender| !sender.is_closed()).await;
    }

    /// Removes current waiters before delivery, so sending holds no map guard.
    pub(crate) fn send(&self, signature: &Signature, status: &TransactionStatus) {
        let Some(senders) = self.senders.remove(signature) else {
            return;
        };
        for sender in senders {
            let _ = sender.send(status.clone());
        }
    }
}

/// Drops abandoned multicast senders after their receivers are gone.
async fn cleanup(subscriptions: Arc<Subscriptions>, mut shutdown: ShutdownHandle) {
    let mut ticker = interval(Duration::from_secs(60));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.signalled() => break,
            _ = ticker.tick() => {
                let _timer = metrics::time(Operation::Cleanup);
                subscriptions.cleanup().await;
            }
        }
    }
    shutdown.terminate(ShutdownReason::Signalled);
}
