//! Live read-side notification channels.

use std::{
    hash::Hash,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use accountsdb::AccountEntry;
use ahash::RandomState;
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

type MpscSenders<V> = SmallVec<[mpsc::Sender<V>; 1]>;
type OneshotSenders<V> = SmallVec<[oneshot::Sender<V>; 1]>;

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
    sender: OnceLock<mpsc::Sender<V>>,
    capacity: usize,
    subscription: Subscription,
}

impl<V> Unicast<V> {
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

    /// Returns whether the stream's process-lifetime receiver is still open.
    pub(crate) fn is_subscribed(&self) -> bool {
        self.sender.get().is_some_and(|sender| !sender.is_closed())
    }

    /// Sends asynchronously, waiting until the receiver has capacity.
    pub(crate) async fn send(&self, value: V) {
        let Some(sender) = self.sender.get() else {
            return;
        };
        let _ = sender.send(value).await;
    }

    /// Sends from a synchronous worker, waiting until the receiver has capacity.
    pub(crate) fn blocking_send(&self, value: V) {
        let Some(sender) = self.sender.get() else {
            return;
        };
        let _ = sender.blocking_send(value);
    }
}

/// Persistent per-key fanout over one bounded queue per receiver.
pub(crate) struct Multicast<K, V> {
    senders: HashMap<K, MpscSenders<V>, RandomState>,
    capacity: usize,
    subscription: Subscription,
}

impl<K, V> Multicast<K, V>
where
    K: Eq + Hash,
{
    pub(crate) fn new(capacity: usize, subscription: Subscription) -> Self {
        Self {
            senders: Default::default(),
            capacity,
            subscription,
        }
    }

    /// Adds a receiver for `key` with its own bounded queue.
    pub(crate) async fn subscribe(&self, key: K) -> mpsc::Receiver<V> {
        let (tx, rx) = mpsc::channel(self.capacity);
        self.senders.entry_async(key).await.or_default().push(tx);
        rx
    }

    /// Adds a receiver synchronously when the public accessor cannot await.
    pub(crate) fn subscribe_sync(&self, key: K) -> mpsc::Receiver<V> {
        let (tx, rx) = mpsc::channel(self.capacity);
        self.senders.entry_sync(key).or_default().push(tx);
        rx
    }

    /// Returns whether `key` has any live receivers.
    pub(crate) fn contains(&self, key: &K) -> bool {
        let mut contains = false;
        self.senders.remove_if_sync(key, |senders| {
            senders.retain(|sender| !sender.is_closed());
            contains = !senders.is_empty();
            !contains
        });
        contains
    }

    /// Drops closed receivers and keys that no longer have receivers.
    async fn cleanup(&self) {
        self.senders
            .retain_async(|_, senders| {
                senders.retain(|sender| !sender.is_closed());
                !senders.is_empty()
            })
            .await;
    }
}

impl<K, V> Multicast<K, V>
where
    K: Eq + Hash,
    V: Clone,
{
    /// Fans out without blocking, disconnecting receivers whose queues are full.
    pub(crate) fn send(&self, key: &K, value: &V) {
        self.senders.remove_if_sync(key, |senders| {
            senders.retain(|sender| match sender.try_send(value.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    metrics::slow_consumer_disconnect(self.subscription);
                    false
                }
                Err(TrySendError::Closed(_)) => false,
            });
            senders.is_empty()
        });
    }
}

/// Terminal per-key fanout over one oneshot channel per receiver.
pub(crate) struct MulticastOneshot<K, V>(HashMap<K, OneshotSenders<V>, RandomState>);

impl<K, V> Default for MulticastOneshot<K, V> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<K, V> MulticastOneshot<K, V>
where
    K: Eq + Hash,
{
    /// Adds a receiver for the terminal value associated with `key`.
    pub(crate) async fn subscribe(&self, key: K) -> oneshot::Receiver<V> {
        let (tx, rx) = oneshot::channel();
        self.0.entry_async(key).await.or_default().push(tx);
        rx
    }

    /// Drops closed receivers and keys that no longer have receivers.
    async fn cleanup(&self) {
        self.0
            // Keep closed positions while any receiver is live so `send_last`
            // cannot mistake an older subscription for the newest one.
            .retain_async(|_, senders| senders.iter().any(|sender| !sender.is_closed()))
            .await;
    }
}

impl<K, V> MulticastOneshot<K, V>
where
    K: Eq + Hash,
    V: Clone,
{
    /// Sends the terminal value only to the most recently added receiver.
    pub(crate) fn send_last(&self, key: &K, value: &V) {
        let Some(mut senders) = self.0.get_sync(key) else {
            return;
        };
        let sender = senders.pop();
        if senders.is_empty() {
            let _ = senders.remove_entry();
        }
        if let Some(sender) = sender {
            let _ = sender.send(value.clone());
        }
    }

    /// Removes `key` and sends its terminal value to every current receiver.
    pub(crate) fn send(&self, key: &K, value: &V) {
        let Some((_, senders)) = self.0.remove_sync(key) else {
            return;
        };
        for sender in senders {
            let _ = sender.send(value.clone());
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
    pub(crate) signatures: MulticastOneshot<Signature, TransactionStatus>,
    /// Log notifications keyed by mentioned program or account pubkey.
    pub(crate) logs: Multicast<Pubkey, Arc<TransactionLogs>>,
    /// Newly committed slots.
    pub(crate) blocks: Multicast<(), Block>,
    /// All committed transactions for the sole stream consumer.
    pub(crate) transactions: Unicast<Arc<FullTransaction>>,
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

    async fn cleanup(&self) {
        self.accounts.cleanup().await;
        self.programs.cleanup().await;
        self.signatures.cleanup().await;
        self.logs.cleanup().await;
        self.blocks.cleanup().await;
        self.snapshots.cleanup().await;
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
