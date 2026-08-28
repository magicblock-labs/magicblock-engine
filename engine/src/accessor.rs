//! Account- and transaction-scoped operation facades.

use std::{collections::BTreeSet, sync::atomic::Ordering, time::Duration};

use keeper::{ExecutionRecord, TransactionView};
use magic_root_interface::MagicRootInstruction;
use processor::{SequencerMessage, Simulation, SimulatorMessage};
use solana_account::{AccountMode, OwnedAccount};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use solana_transaction::TransactionResult;
use tokio::time;

use crate::{
    Engine, IntoTransactionView,
    error::{EngineError, Result},
    transaction::{self, VerifiedTransaction},
};

/// Upper bound on awaiting a submitted transaction's committed result.
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(8);

/// Account-scoped operations bound to a single `pubkey`.
pub struct AccountAccessor<'a> {
    pub(crate) pubkey: Pubkey,
    pub(crate) engine: &'a Engine,
}

/// Transaction-submission operations bound to an engine instance.
pub struct TransactionAccessor<'a> {
    pub(crate) engine: &'a Engine,
    pub(crate) transaction: TransactionView,
}

impl AccountAccessor<'_> {
    /// Creates the account by patching in every field and finalizing it,
    /// optionally running follow-up `actions` once it is finalized.
    ///
    /// Missing writable accounts named by `actions` (ER-only PDAs such as an
    /// auction tree) are asserted with [`MagicRootInstruction::Prepare`] in the
    /// same transaction, so a follow-up action can initialize them (for example
    /// `create_ephemeral`). `Prepare` materializes the placeholder only if the
    /// account still does not exist at execution time, so concurrent creates
    /// that share such an account converge on the first materialization instead
    /// of failing each other.
    pub async fn create(
        &self,
        acc: impl Into<OwnedAccount>,
        actions: Option<Vec<Instruction>>,
    ) -> Result<()> {
        let mut instructions = Vec::new();
        if let Some(actions) = actions.as_ref() {
            for pubkey in self.missing_writable_action_accounts(actions) {
                instructions.push(MagicRootInstruction::Prepare.compose(pubkey)?);
            }
        }
        instructions.extend(MagicRootInstruction::compose_account(self.pubkey, acc.into())?);
        if let Some(actions) = actions {
            instructions.push(MagicRootInstruction::PostFinalize(actions).compose(self.pubkey)?);
        }
        self.execute(instructions).await
    }

    /// Writable action accounts that are absent or only a slot-0 Placeholder.
    fn missing_writable_action_accounts(&self, actions: &[Instruction]) -> Vec<Pubkey> {
        let accounts = self.engine.accounts();
        let loader = accounts.loader();
        let mut seen = BTreeSet::new();
        let mut missing = Vec::new();
        for action in actions {
            for meta in &action.accounts {
                if !meta.is_writable || meta.pubkey == self.pubkey || !seen.insert(meta.pubkey) {
                    continue;
                }
                let needs_prepare = loader
                    .load(&meta.pubkey)
                    .ok()
                    .flatten()
                    .is_none_or(|account| {
                        account.is(AccountMode::Placeholder) && account.slot() == 0
                    });
                if needs_prepare {
                    missing.push(meta.pubkey);
                }
            }
        }
        missing
    }

    /// Updates the account by patching in every field of `account`
    pub async fn update(&self, acc: impl Into<OwnedAccount>) -> Result<()> {
        let instructions = MagicRootInstruction::compose_account(self.pubkey, acc.into())?;
        self.execute(instructions).await
    }

    /// Closes the account.
    pub async fn delete(&self) -> Result<()> {
        let instructions = vec![MagicRootInstruction::Delete.compose(self.pubkey)?];
        self.execute(instructions).await
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
        let signature = self.transaction.signatures()[0];
        let msg = SequencerMessage::Transaction(self.transaction);
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
        let msg = SequencerMessage::Transaction(self.transaction);
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
