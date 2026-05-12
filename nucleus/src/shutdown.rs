//! Cooperative shutdown for engine services.
//!
//! A [`ShutdownManager`] owns ordered cancellation tokens and a set of
//! registered service handles. Each service receives a [`ShutdownHandle`],
//! observes its tier token while running, and reports a [`ShutdownReason`] when
//! it exits. The manager waits for an OS shutdown signal, internal cancellation,
//! or service termination, then cancels each tier in order and gives it a
//! bounded window to stop before moving on.

use std::{
    error::Error,
    time::{Duration, Instant},
};

use futures::{
    StreamExt,
    future::{BoxFuture, select},
    stream::FuturesUnordered,
};
use oneshot::Sender;
use tokio::time::timeout;
use tracing::{error, info, warn};

pub use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(4);

type HandleFuture = BoxFuture<'static, (Service, ShutdownTier, ShutdownReason)>;

/// Background service tracked by the shutdown manager.
#[derive(Clone, Copy, Debug)]
pub enum Service {
    /// Ledger append worker.
    LedgerAppender,
    /// Ledger read worker.
    LedgerReader,
    /// Ledger replay worker.
    LedgerReplayer,
    /// Transaction scheduler service.
    Sequencer,
    /// Transaction executor worker with its worker index.
    TransactionExecutor(u32),
    /// Transaction simulation worker.
    TransactionSimulator,
    /// Subscription map cleanup worker.
    SubscriptionsCleanup,
    /// Block pacing task.
    PaceMaker,
    /// Leader-side service streaming blockstore bytes to followers.
    ReplicationDispatcher,
    /// Follower-side service pulling replicated state from the leader.
    ReplicationClient,
}

impl Service {
    /// Shutdown tier for this service; lower tiers are stopped first.
    fn tier(&self) -> ShutdownTier {
        use Service::*;
        match self {
            ReplicationClient => ShutdownTier::One,
            PaceMaker => ShutdownTier::Two,
            // The pacemaker drains the sequencer and sends the appender's final
            // sync before either service reaches this tier.
            Sequencer | LedgerAppender => ShutdownTier::Three,
            _ => ShutdownTier::Four,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ShutdownTier {
    One,
    Two,
    Three,
    Four,
}

impl ShutdownTier {
    const COUNT: usize = 4;
    const ORDER: [Self; Self::COUNT] = [Self::One, Self::Two, Self::Three, Self::Four];
}

/// Coordinates graceful shutdown across engine services.
#[derive(Default)]
pub struct ShutdownManager {
    /// Service cancellation tokens, one per ordered shutdown tier.
    tokens: [CancellationToken; ShutdownTier::COUNT],
    /// Registered service termination reports.
    handles: FuturesUnordered<HandleFuture>,
    /// Number of services that have not reported termination, by tier.
    pending: [isize; ShutdownTier::COUNT],
}

impl ShutdownManager {
    /// Wait for an OS shutdown signal, internal cancellation, or service failure.
    pub async fn wait(&mut self) -> ShutdownReason {
        tokio::select! {
            _ = graceful_shutdown() => {
                info!("graceful shutdown has been requested");
                ShutdownReason::Signalled
            }
            Some((service, tier, reason)) = self.handles.next(), if !self.handles.is_empty() => {
                self.pending(tier, -1);
                error!(?service, ?reason, "terminated prematurely");
                reason
            }
        }
    }

    /// Cancels services one tier at a time and drains their termination reports.
    ///
    /// Each tier gets `TIMEOUT` to report before the next tier is
    /// cancelled. Already terminated services are skipped by their tier.
    pub async fn terminate(&mut self) {
        info!("initiating graceful shutdown of the engine");
        let start = Instant::now();
        let mut timers = [start; ShutdownTier::COUNT];
        for tier in ShutdownTier::ORDER {
            timers[tier as usize] = Instant::now();
            self.tokens[tier as usize].cancel();
            if self.pending[tier as usize] == 0 {
                continue;
            }
            if timeout(TIMEOUT, self.drain(tier, &timers)).await.is_err() {
                let remaining = self.pending[tier as usize];
                let elapsed = timers[tier as usize].elapsed();
                warn!(?tier, remaining, ?elapsed, "shutdown tier timed out");
            }
        }
        info!(elapsed = ?start.elapsed(), "engine shutdown complete");
    }

    /// Register a service and return its cancellation handle.
    pub fn handle(&mut self, service: Service) -> ShutdownHandle {
        let tier = service.tier();
        let (tx, rx) = oneshot::channel();
        let fut = async move {
            let reason = rx.await.unwrap_or_default();
            (service, tier, reason)
        };
        self.handles.push(Box::pin(fut));
        self.pending(tier, 1);
        ShutdownHandle {
            cancel: self.tokens[tier as usize].child_token(),
            reason: Some(tx),
        }
    }

    async fn drain(&mut self, tier: ShutdownTier, timers: &[Instant]) {
        while self.pending[tier as usize] != 0 {
            let Some((service, tier, reason)) = self.handles.next().await else {
                return;
            };
            // Another tier may finish while this one drains; debit its own pending count.
            self.pending(tier, -1);
            let elapsed = timers[tier as usize].elapsed();
            Self::log(service, reason, elapsed);
        }
    }

    fn pending(&mut self, tier: ShutdownTier, op: isize) {
        self.pending[tier as usize] += op;
    }

    fn log(service: Service, reason: ShutdownReason, elapsed: Duration) {
        match reason {
            ShutdownReason::Unexpected => {
                warn!(?service, ?elapsed, "terminated unexpectedly")
            }
            ShutdownReason::Signalled => {
                info!(?service, ?elapsed, "terminated gracefully")
            }
            ShutdownReason::RestartRequired => {
                warn!(?service, ?elapsed, "requested a restart")
            }
            ShutdownReason::Error(error) => {
                error!(?service, ?error, ?elapsed, "terminated with error")
            }
        }
    }
}

/// Waits for SIGTERM or Ctrl-C.
async fn graceful_shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let term = Box::pin(async {
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            return;
        };
        term.recv().await;
    });
    let ctrlc = Box::pin(tokio::signal::ctrl_c());
    select(term, ctrlc).await;
}

/// Cancellation handle held by a running service.
pub struct ShutdownHandle {
    /// Token observed by the running service.
    cancel: CancellationToken,
    /// One-shot report consumed by the manager when the service exits.
    reason: Option<Sender<ShutdownReason>>,
}

/// Reason reported when a service terminates.
#[derive(Debug, Default)]
pub enum ShutdownReason {
    /// Service handle was dropped without reporting a reason.
    #[default]
    Unexpected,
    /// Service stopped after being signalled.
    Signalled,
    /// Service stopped because of an error.
    Error(Box<dyn Error + Send + Sync + 'static>),
    /// Service staged state that must be installed by restarting the engine.
    RestartRequired,
}

impl ShutdownHandle {
    /// Request engine shutdown and report this service's termination reason.
    pub fn terminate(&mut self, reason: ShutdownReason) {
        self.cancel.cancel();
        if let Some(tx) = self.reason.take() {
            let _ = tx.send(reason);
        };
    }

    /// Wait until this service's shutdown tier is cancelled.
    pub async fn signalled(&self) {
        self.cancel.cancelled().await
    }

    /// Returns whether this service's shutdown tier has been cancelled.
    pub fn requested(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Creates a cancellation token for work owned by this service.
    pub fn child(&self) -> CancellationToken {
        self.cancel.child_token()
    }
}

impl Drop for ShutdownHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.reason.take() {
            let _ = tx.send(ShutdownReason::Unexpected);
        };
    }
}
