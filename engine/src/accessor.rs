//! Account- and transaction-scoped operation facades.

use std::sync::atomic::Ordering;

use derive_more::Deref;
use keeper::{AccountLease, ExecutionRecord, ResolvedTransaction, TransactionView};
use magic_root_interface::{ACCOUNT_CHANGED, MagicRootInstruction, PostFinalize};
use nucleus::runtime::ExecutionRequest;
use processor::{SequencerMessage, Simulation, SimulatorMessage};
use solana_account::{AccountFieldPatch, AccountMode, OwnedAccount};
use solana_instruction::Instruction;
use solana_transaction::{TransactionError, TransactionResult};

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
#[derive(Deref)]
pub struct AccountAccessor<'a> {
    /// Exclusive account reservation and its post-acquisition lifecycle observation.
    #[deref]
    pub(crate) lease: AccountLease,
    pub(crate) engine: &'a Engine,
    pub(crate) expected: Option<OwnedAccount>,
}

/// Transaction-submission operations bound to an engine instance.
pub struct TransactionAccessor<'a> {
    pub(crate) engine: &'a Engine,
    /// Sanitized transaction with private-transaction authority checked.
    pub(crate) transaction: TransactionView,
    /// Verify signatures before execution unless ingress or replay is trusted.
    pub(crate) verify: bool,
}

impl AccountAccessor<'_> {
    /// Conditions materialization on this complete account image.
    /// A default image represents absence. A conflict changes nothing and runs
    /// no actions; reacquire, reread, and recompute before retrying.
    pub fn require(mut self, expected: &OwnedAccount) -> Self {
        self.expected = Some(expected.clone());
        self
    }

    /// Materializes the account by patching in every field and finalizing it,
    /// optionally running follow-up actions once it is finalized.
    ///
    /// Callers supplying `actions` must verify its trusted provenance as
    /// required by [`PostFinalize`] before invoking this method.
    /// Confirmed redelegation replaces `Undelegating` directly with `Delegated`
    /// at a strictly newer remote slot. The caller must establish a new
    /// delegation generation, not merely a newer observation of the old one,
    /// and reconcile local state after acquiring this accessor.
    /// See the crate's account replacement contract for caller evidence.
    ///
    /// Returns success without submitting an older image or one with the same
    /// slot and mode as the lease observation. In that case follow-up actions
    /// do not run, even if the image data differs. `Ok(())` therefore means
    /// applied or skipped as already handled or superseded.
    ///
    /// Patches, finalization, and actions share one transaction; an execution
    /// failure rolls back their account changes. Retrying requires reacquiring
    /// the account and reconciling its state.
    /// There is no internal deadline. Cancelling this wait does not cancel
    /// submitted execution; Engine retains ownership through completion and
    /// recency bookkeeping. Reacquire and reconcile before deciding on recovery.
    pub async fn materialize(
        mut self,
        acc: impl Into<OwnedAccount>,
        actions: Option<PostFinalize>,
    ) -> Result<()> {
        let acc = acc.into();
        let mode = acc.mode();
        if let Some(local_mode) = self.skipped(mode, acc.slot()) {
            self.lease.materialized(local_mode);
            return Ok(());
        }
        let pubkey = self.pubkey();
        let lamports = acc.lamports();
        let mut instructions = MagicRootInstruction::compose_account(pubkey, acc)?;
        if let Some(expected) = self.expected.take() {
            // compose_account starts with the lamport patch. Fold the compare
            // into it so conditional materialization adds no execution step.
            instructions[0] = MagicRootInstruction::CompareAndPatch {
                expected,
                patch: AccountFieldPatch::Lamports(lamports),
            }
            .compose(pubkey)?;
        }
        if let Some(actions) = actions {
            let ix = MagicRootInstruction::PostFinalize(actions);
            instructions.push(ix.compose(pubkey)?);
        }
        self.execute(instructions, Some(mode)).await
    }

    /// Closes the account, releasing ownership after definitive completion.
    /// Cancellation has the same ownership contract as [`Self::materialize`].
    pub async fn delete(self) -> Result<()> {
        let instruction = MagicRootInstruction::Delete.compose(self.pubkey())?;
        self.execute(vec![instruction], None).await
    }

    /// Keeps this accessor only for a present account that does not require persistence.
    pub fn into_eviction(self) -> Option<Self> {
        self.evictable().then_some(self)
    }

    /// Returns this accessor only if an earlier cache eviction still applies.
    pub fn into_cached_eviction(self) -> Option<Self> {
        self.cached_eviction_applies().then_some(self)
    }

    /// Executes authority-signed account maintenance, retaining the lease through
    /// definitive completion even if the caller cancels its wait.
    /// The caller validates freshness and replacement eligibility. On success,
    /// `mode` must describe the target's resulting lifecycle, or be `None` for deletion.
    async fn execute(
        self,
        instructions: Vec<Instruction>,
        mode: Option<AccountMode>,
    ) -> Result<()> {
        let txn = transaction::magicblock(instructions, self.engine)?;
        let rx = self.engine.transaction(txn)?.submit().await?;
        // No await may separate successful submission from this lease handoff.
        let mut lease = self.lease;
        // Dropping the join handle detaches this task; it must never be aborted.
        let result = tokio::spawn(async move {
            rx.await??;
            match mode {
                Some(mode) => lease.materialized(mode),
                None => lease.deleted(),
            }
            Ok(())
        })
        .await?;
        const ACCOUNT_CHANGED_ERROR: TransactionError =
            TransactionError::InstructionError(0, ACCOUNT_CHANGED);
        match result {
            Err(EngineError::TransactionExecution(ACCOUNT_CHANGED_ERROR)) => {
                Err(EngineError::AccountChanged)
            }
            result => result,
        }
    }
}

impl<'a> TransactionAccessor<'a> {
    /// Composes a trusted local-ledger transaction without verifying its signatures.
    pub(super) fn replay(engine: &'a Engine, transaction: Vec<u8>) -> Result<Self> {
        Ok(Self {
            engine,
            transaction: transaction.compose(engine)?,
            verify: false,
        })
    }

    /// Enters the trusted replication path without repeating signature verification.
    ///
    /// The caller must only pass values produced by this Engine's verifier.
    pub fn verified(engine: &'a Engine, verified: VerifiedTransaction) -> Self {
        Self {
            engine,
            transaction: verified.0,
            verify: false,
        }
    }

    /// Submits `transaction` and awaits admission rejection or its committed result.
    /// Ordinary inputs must pass signature verification before queueing.
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
    /// Ordinary inputs must pass signature verification before queueing.
    /// Success acknowledges queueing, not admission; rejected work is dropped.
    pub async fn schedule(self) -> Result<()> {
        self.enqueue(None).await
    }

    /// Resolves and queues work, transferring any reply channel to the sequencer.
    async fn enqueue(self, response: Option<oneshot::Sender<TransactionResult<()>>>) -> Result<()> {
        if self.engine.terminating.load(Ordering::Acquire) {
            return Err(EngineError::ShuttingDown);
        }
        if self.verify {
            transaction::sigverify(&self.transaction)?;
        }
        let transaction =
            ResolvedTransaction::try_new(self.transaction, None, &Default::default())?;
        let msg = SequencerMessage::Transaction(ExecutionRequest { transaction, response });
        self.engine.sequencer.send(msg).await.map_err(Into::into)
    }

    /// Simulates `transaction` against current state without committing it.
    /// Verifies transaction signatures before queueing only when `sigverify` is true.
    pub async fn simulate(self, sigverify: bool) -> Result<TransactionResult<ExecutionRecord>> {
        if self.engine.terminating.load(Ordering::Acquire) {
            return Err(EngineError::ShuttingDown);
        }
        if sigverify {
            transaction::sigverify(&self.transaction)?;
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
