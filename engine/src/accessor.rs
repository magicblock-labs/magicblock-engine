//! Account- and transaction-scoped operation facades.

use std::{sync::atomic::Ordering, time::Duration};

use keeper::{
    AccountLease, ExecutionRecord, ResolvedTransaction, TransactionView, error::KeeperError,
};
use magic_root_interface::{MagicRootInstruction, PostFinalize};
use processor::{SequencerMessage, Simulation, SimulatorMessage};
use solana_account::{AccountMode, AccountSharedData, OwnedAccount};
use solana_instruction::Instruction;
use solana_transaction::TransactionResult;
use tokio::time;

use crate::{
    Engine, IntoTransactionView,
    error::{EngineError, Result},
    transaction::{self, VerifiedTransaction},
};

/// Upper bound on awaiting a submitted transaction's committed result.
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(8);

/// Exclusive mutation access to one account.
///
/// Dropping the accessor releases the account for the next caller. Keeping it
/// across a failed operation allows a serialized fallback attempt.
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
    /// The accessor retains mutation ownership after both success and failure,
    /// and may be reused before it is dropped.
    pub async fn materialize(
        &mut self,
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
        self.execute(instructions).await?;
        self.lease.materialized(mode).await;
        Ok(())
    }

    /// Closes the account.
    pub async fn delete(&mut self) -> Result<()> {
        let pubkey = self.lease.pubkey();
        let instructions = vec![MagicRootInstruction::Delete.compose(pubkey)?];
        self.execute(instructions).await?;
        self.lease.deleted();
        Ok(())
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

    /// Composes the instructions into a signed engine transaction, executes it,
    /// and flattens the committed transaction result into the engine error type.
    async fn execute(&self, instructions: Vec<Instruction>) -> Result<()> {
        let txn = transaction::magicblock(&instructions, self.engine)?;
        self.engine.transaction(txn)?.execute().await?.map_err(Into::into)
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

    /// Submits `transaction` for execution and awaits its committed result.
    /// A timeout does not cancel the submitted transaction.
    pub async fn execute(self) -> Result<TransactionResult<()>> {
        if self.engine.terminating.load(Ordering::Acquire) {
            return Err(EngineError::ShuttingDown);
        }
        let transaction =
            ResolvedTransaction::try_new(self.transaction, None, &Default::default())?;
        let signature = transaction.signatures()[0];
        let msg = SequencerMessage::Transaction(transaction);
        let rx = self.engine.transactions().subscribe_signature(signature).await;
        self.engine.sequencer.send(msg).await?;
        let status = time::timeout(EXECUTION_TIMEOUT, rx)
            .await
            .map_err(|_| EngineError::TransactionTimeout)?
            .map_err(|e| e.to_string())?;
        Ok(status.result)
    }

    /// Submits `transaction` for execution without awaiting its result.
    pub async fn schedule(self) -> Result<()> {
        if self.engine.terminating.load(Ordering::Acquire) {
            return Err(EngineError::ShuttingDown);
        }
        let transaction =
            ResolvedTransaction::try_new(self.transaction, None, &Default::default())?;
        let msg = SequencerMessage::Transaction(transaction);
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
