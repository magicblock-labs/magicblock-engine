//! Account- and transaction-scoped operation facades.

use std::{sync::atomic::Ordering, time::Duration};

use keeper::{ExecutionRecord, TransactionView};
use magic_root_interface::MagicRootInstruction;
use processor::{SequencerMessage, Simulation, SimulatorMessage};
use solana_account::{AccountFieldPatch, OwnedAccount};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use solana_transaction::TransactionResult;
use tokio::time;

use crate::{Engine, error::EngineError, error::Result, transaction};

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
    pub async fn create(
        &self,
        acc: impl Into<OwnedAccount>,
        actions: Option<Vec<Instruction>>,
    ) -> Result<()> {
        let mut instructions = MagicRootInstruction::compose_account(self.pubkey, acc.into())?;
        if let Some(actions) = actions {
            instructions.push(MagicRootInstruction::PostFinalize(actions).compose(self.pubkey)?);
        }
        self.execute(instructions).await
    }

    /// Updates the account by patching in every field of `account`
    pub async fn update(&self, acc: impl Into<OwnedAccount>) -> Result<()> {
        let instructions = MagicRootInstruction::compose_account(self.pubkey, acc.into())?;
        self.execute(instructions).await
    }

    /// Applies the given field patches to the account.
    pub async fn patch(&self, patches: Vec<AccountFieldPatch>) -> Result<()> {
        let mut instructions = Vec::with_capacity(patches.len());
        for patch in patches {
            let ix = MagicRootInstruction::Patch(patch).compose(self.pubkey)?;
            instructions.push(ix);
        }
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

impl TransactionAccessor<'_> {
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
