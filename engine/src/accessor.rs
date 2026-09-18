//! Account- and transaction-scoped operation facades.

use std::sync::atomic::Ordering;

use keeper::{
    AccountLease, ExecutionRecord, ResolvedTransaction, TransactionView, error::KeeperError,
};
use magic_root_interface::{MagicRootInstruction, PostFinalize};
use nucleus::runtime::ExecutionRequest;
use processor::{SequencerMessage, Simulation, SimulatorMessage};
use solana_account::{AccountMode, AccountSharedData, OwnedAccount};
use solana_instruction::Instruction;
use solana_transaction::TransactionResult;

use crate::{
    Engine, IntoTransactionView,
    error::{EngineError, Result},
    transaction::{self, VerifiedTransaction},
};

/// Exclusive mutation access to one account.
///
/// Mutations consume the accessor and release it after definitive completion.
/// Cancelling a submitted mutation's wait leaves its lease with Engine until
/// completion; dropping an idle accessor releases it immediately.
pub struct AccountAccessor<'a> {
    pub(crate) lease: AccountLease,
    pub(crate) engine: &'a Engine,
}

/// Transaction-submission operations bound to an engine instance.
pub struct TransactionAccessor<'a> {
    pub(crate) engine: &'a Engine,
    pub(crate) transaction: TransactionView,
}

impl AccountAccessor<'_> {
    /// Reads the current account without copying its backing data.
    /// `reader` may run more than once if a concurrent publish changes the image.
    pub fn read<R>(&self, reader: impl Fn(&AccountSharedData) -> R) -> Result<Option<R>> {
        self.engine
            .accounts()
            .loader()
            .read(&self.lease.pubkey(), reader)
            .map_err(|err| EngineError::from(KeeperError::from(err)))
    }

    /// Materializes the account by patching in every field and finalizing it,
    /// optionally running follow-up actions once it is finalized.
    ///
    /// Callers supplying `post_finalize` must verify its trusted provenance as
    /// required by [`PostFinalize`] before invoking this method.
    /// Confirmed redelegation replaces `Transient` directly with `Delegated`
    /// at a strictly newer remote slot. The caller must establish a new
    /// delegation generation, not merely a newer observation of the old one,
    /// and recheck local state through [`Self::read`] after acquiring this accessor.
    /// See the crate's account replacement contract for caller evidence.
    ///
    /// Patches, finalization, and actions share one transaction; an execution
    /// failure rolls back their account changes. Retrying requires reacquiring
    /// the account and rechecking its state.
    /// There is no internal deadline. Cancelling this wait does not cancel
    /// submitted execution; Engine retains ownership through completion and
    /// recency bookkeeping. Reacquire and reread before deciding on recovery.
    pub async fn materialize(
        self,
        acc: impl Into<OwnedAccount>,
        post_finalize: Option<PostFinalize>,
    ) -> Result<()> {
        let pubkey = self.lease.pubkey();
        let acc = acc.into();
        let mode = acc.mode();
        let mut instructions = MagicRootInstruction::compose_account(pubkey, acc)?;
        if let Some(post_finalize) = post_finalize {
            let ix = MagicRootInstruction::PostFinalize(post_finalize);
            instructions.push(ix.compose(pubkey)?);
        }
        self.execute(&instructions, Some(mode)).await
    }

    /// Closes the account, releasing ownership after definitive completion.
    /// Cancellation has the same ownership contract as [`Self::materialize`].
    pub async fn delete(self) -> Result<()> {
        let pubkey = self.lease.pubkey();
        let instruction = MagicRootInstruction::Delete.compose(pubkey)?;
        self.execute(&[instruction], None).await
    }

    /// Releases a satisfied request, promoting non-authoritative state in
    /// recency before this accessor is dropped.
    pub async fn satisfy(self, mode: AccountMode) {
        if !mode.authoritative() {
            self.lease.materialized(mode).await;
        }
    }

    /// Returns this accessor only if an earlier cache eviction still applies.
    pub fn into_cached_eviction(self, mode: AccountMode) -> Option<Self> {
        self.lease.cached_eviction_applies(mode).then_some(self)
    }

    /// Before submission, cancellation releases the lease without submitting work.
    /// After submission, the task owns completion and success bookkeeping.
    async fn execute(self, instructions: &[Instruction], mode: Option<AccountMode>) -> Result<()> {
        let txn = transaction::magicblock(instructions, self.engine)?;
        let rx = self.engine.transaction(txn)?.submit().await?;
        // No await may separate successful submission from this lease handoff.
        let lease = self.lease;
        // Dropping the join handle detaches this task; it must never be aborted.
        tokio::spawn(async move {
            rx.await??;
            match mode {
                Some(mode) => lease.materialized(mode).await,
                None => lease.deleted(),
            }
            Ok(())
        })
        .await?
    }
}

impl<'a> TransactionAccessor<'a> {
    /// Composes a trusted local-ledger transaction without verifying its signatures.
    pub(super) fn replay(engine: &'a Engine, transaction: Vec<u8>) -> Result<Self> {
        let sanitized = TransactionView::try_new_sanitized(transaction.into(), true)?;
        let transaction = sanitized.compose(engine)?;
        Ok(Self { engine, transaction })
    }

    /// Enters the trusted replication path without repeating signature verification.
    ///
    /// The caller must only pass values produced by this Engine's verifier.
    pub fn verified(engine: &'a Engine, verified: VerifiedTransaction) -> Self {
        Self { engine, transaction: verified.0 }
    }

    /// Submits `transaction` and awaits admission rejection or its committed result.
    /// There is no internal deadline: submitted execution either publishes a
    /// request-specific result or the host shuts down the process on an
    /// infrastructure failure. Cancelling this wait does not cancel the transaction.
    pub async fn execute(self) -> Result<TransactionResult<()>> {
        Ok(self.submit().await?.await?)
    }

    /// Transfers completion ownership with the submission, without subscribing.
    async fn submit(self) -> Result<oneshot::Receiver<TransactionResult<()>>> {
        let (response, rx) = oneshot::channel();
        self.enqueue(Some(response)).await?;
        Ok(rx)
    }

    /// Submits `transaction` for execution without awaiting its result.
    /// Success acknowledges queueing, not admission; rejected work is dropped.
    pub async fn schedule(self) -> Result<()> {
        self.enqueue(None).await
    }

    /// Resolves and queues work, transferring any reply channel to the sequencer.
    async fn enqueue(self, response: Option<oneshot::Sender<TransactionResult<()>>>) -> Result<()> {
        if self.engine.terminating.load(Ordering::Acquire) {
            return Err(EngineError::ShuttingDown);
        }
        let transaction =
            ResolvedTransaction::try_new(self.transaction, None, &Default::default())?;
        let msg = SequencerMessage::Transaction(ExecutionRequest { transaction, response });
        self.engine.sequencer.send(msg).await.map_err(Into::into)
    }

    /// Simulates `transaction` against current state without committing it.
    pub async fn simulate(self) -> Result<TransactionResult<ExecutionRecord>> {
        if self.engine.terminating.load(Ordering::Acquire) {
            return Err(EngineError::ShuttingDown);
        }
        let (response, rx) = oneshot::channel();
        let msg = SimulatorMessage::Transaction(Simulation {
            transaction: self.transaction,
            response,
        });
        self.engine.sequencer.simulation.send(msg).await?;
        rx.await.map_err(Into::into)
    }
}
