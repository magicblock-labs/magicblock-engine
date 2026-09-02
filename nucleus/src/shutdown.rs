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
    io, mem,
    time::{Duration, Instant},
};

use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use oneshot::Sender;
use tokio::time::timeout;
use tracing::{error, info, warn};

pub use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(4);

type HandleFuture = BoxFuture<'static, (Service, ShutdownTier, ShutdownReason)>;

/// Background service tracked by the shutdown manager.
#[derive(Clone, Copy, Debug)]
pub enum Service {
    /// Leader JSON-RPC and WebSocket ingress.
    Rpc,
    /// Process metrics endpoint.
    Metrics,
    /// Leader base-chain startup setup.
    OnchainSetup,
    /// Program-scheduled task service.
    TaskScheduler,
    /// Scheduled base-chain intent execution service.
    IntentExecution,
    /// Observed undelegation request service.
    UndelegationRequests,
    /// Periodic validator fee claiming service.
    FeeClaim,
    /// Ledger append worker.
    LedgerAppender,
    /// Ledger index worker.
    LedgerIndexer,
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
            Rpc | OnchainSetup | TaskScheduler | IntentExecution | UndelegationRequests
            | FeeClaim | ReplicationClient => ShutdownTier::One,
            PaceMaker => ShutdownTier::Two,
            // The pacemaker drains the sequencer and sends the appender's final
            // sync before either service reaches this tier.
            Sequencer | LedgerAppender | LedgerIndexer => ShutdownTier::Three,
            Metrics => ShutdownTier::Four,
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
            result = graceful_shutdown() => {
                match result {
                    Ok(()) => {
                        info!("graceful shutdown has been requested");
                        ShutdownReason::Signalled
                    }
                    Err(error) => ShutdownReason::Error(Box::new(error)),
                }
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
    pub async fn terminate(&mut self) -> ShutdownReason {
        info!("initiating graceful shutdown of the engine");
        let start = Instant::now();
        let mut timers = [start; ShutdownTier::COUNT];
        let mut outcome = ShutdownReason::Signalled;
        for tier in ShutdownTier::ORDER {
            timers[tier as usize] = Instant::now();
            self.tokens[tier as usize].cancel();
            if self.pending[tier as usize] == 0 {
                continue;
            }
            if let Err(e) = timeout(TIMEOUT, self.drain(tier, &timers, &mut outcome)).await {
                let remaining = self.pending[tier as usize];
                let elapsed = timers[tier as usize].elapsed();
                warn!(?tier, remaining, ?elapsed, "shutdown tier timed out");
                let error = Box::new(io::Error::from(e));
                outcome = outcome.combine(ShutdownReason::Error(error));
            }
        }
        info!(elapsed = ?start.elapsed(), "engine shutdown complete");
        outcome
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

    async fn drain(
        &mut self,
        tier: ShutdownTier,
        timers: &[Instant],
        outcome: &mut ShutdownReason,
    ) {
        while self.pending[tier as usize] != 0 {
            let Some((service, tier, reason)) = self.handles.next().await else {
                return;
            };
            // Another tier may finish while this one drains; debit its own pending count.
            self.pending(tier, -1);
            let elapsed = timers[tier as usize].elapsed();
            Self::log(service, &reason, elapsed);
            *outcome = mem::take(outcome).combine(reason);
        }
    }

    fn pending(&mut self, tier: ShutdownTier, op: isize) {
        self.pending[tier as usize] += op;
    }

    fn log(service: Service, reason: &ShutdownReason, elapsed: Duration) {
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

impl Drop for ShutdownManager {
    fn drop(&mut self) {
        for token in &self.tokens {
            token.cancel();
        }
    }
}

/// Waits for SIGTERM or Ctrl-C.
async fn graceful_shutdown() -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        signal = term.recv() => signal
            .ok_or_else(|| io::Error::other("SIGTERM listener closed")),
        result = tokio::signal::ctrl_c() => result,
    }
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

impl ShutdownReason {
    /// Combines independently observed shutdown reasons into one process outcome.
    ///
    /// The first concrete error is retained ahead of an unexpected exit, a
    /// requested restart, or a clean signal. This lets callers wait for the
    /// first terminating service, drain every remaining service, and decide the
    /// process outcome without losing a later failure.
    pub fn combine(self, next: Self) -> Self {
        if next.exit_code() > self.exit_code() { next } else { self }
    }

    /// Returns the stable process exit code for this shutdown reason.
    ///
    /// `0` denotes a clean signal, `1` requests a restart, `2` denotes an
    /// unexpected service exit, and `3` preserves a concrete service error.
    /// [`Self::combine`] also uses this ordering to retain the strongest reason.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Signalled => 0,
            Self::RestartRequired => 1,
            Self::Unexpected => 2,
            Self::Error(_) => 3,
        }
    }
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
