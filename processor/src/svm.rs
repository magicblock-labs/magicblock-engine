//! Shared SVM execution context for the transaction executor and simulator.

use std::sync::Arc;

use agave_feature_set::FeatureSet;
use agave_transaction_view::{
    MAGICBLOCK_INSTRUCTION_TRACE_LENGTH, transaction_version::TransactionVersion,
};
use keeper::{Keeper, ResolvedTransaction};
use nucleus::{Slot, ledger::Block};
use solana_compute_budget_instruction::instructions_processor::process_compute_budget_instructions;
use solana_program_runtime::loaded_programs::{ProgramCache, ProgramRuntimeEnvironments};
use solana_svm::{
    account_loader::CheckedTransactionDetails,
    transaction_processing_callback::TransactionProcessingCallback,
    transaction_processor::{
        ExecutionRecordingConfig, LoadAndExecuteSanitizedTransactionOutput,
        TransactionBatchProcessor, TransactionProcessingConfig, TransactionProcessingEnvironment,
    },
};
use solana_sysvar::clock::Clock;
use solana_transaction_error::TransactionResult;

use crate::{Result, callback::SVMCallback};

/// SVM batch processor together with its per-block environment and recording
/// config, shared verbatim by the executor and simulator workers.
pub(crate) struct SvmContext {
    /// SVM batch processor that loads and executes transactions.
    processor: TransactionBatchProcessor,
    /// Per-block processing environment (blockhash, features, rent, ...).
    env: TransactionProcessingEnvironment,
    /// Recording and logging configuration for execution.
    config: TransactionProcessingConfig,
}

impl SvmContext {
    /// Builds the SVM runtime environment and batch processor from current state.
    pub(crate) fn new(state: &Keeper, cache: Arc<ProgramCache>) -> Result<Self> {
        let budget = Default::default();
        let runtime_env = agave_syscalls::create_program_runtime_environment(
            &state.features().runtime_features(),
            &budget,
            true,
            false,
        )
        .map_err(|e| e.to_string())?;
        let envs = ProgramRuntimeEnvironments::new(runtime_env);
        let block = state.blocks().latest();
        let env = TransactionProcessingEnvironment {
            blockhash: block.hash,
            feature_set: state.features().runtime_features(),
            program_runtime_environments_for_execution: envs,
            rent: state.rent().clone(),
            blockhash_lamports_per_signature: 0,
            epoch_total_stake: 0,
        };
        let mut processor = TransactionBatchProcessor::new(block.slot + 1, cache);
        let accessor = state.accounts();
        let callback = SVMCallback::<false> {
            loader: accessor.loader(),
            featureset: state.features(),
        };
        processor.fill_missing_sysvar_cache_entries(&callback);
        let config = TransactionProcessingConfig {
            log_messages_bytes_limit: None,
            recording_config: ExecutionRecordingConfig::new_single_setting(true),
        };
        Ok(Self { processor, env, config })
    }

    /// Loads and executes a single transaction through the SVM.
    pub(crate) fn execute(
        &self,
        callback: &impl TransactionProcessingCallback,
        txn: &ResolvedTransaction,
        features: &FeatureSet,
    ) -> LoadAndExecuteSanitizedTransactionOutput {
        let details = match self.parse_details(txn, features) {
            Ok(d) => d,
            Err(e) => {
                return LoadAndExecuteSanitizedTransactionOutput {
                    processing_result: Err(e),
                    balance_collector: None,
                };
            }
        };
        self.processor.load_and_execute_sanitized_transaction(
            callback,
            txn,
            details,
            &self.env,
            &self.config,
        )
    }

    /// Advances the context to a new block: bumps the slot and blockhash and
    /// refreshes the cached clock sysvar.
    pub(crate) fn transition(&mut self, block: Block) {
        let slot = block.slot + 1;
        let hash = block.hash;
        self.processor.slot = slot;
        self.env.blockhash = hash;
        let clock = Clock {
            slot,
            unix_timestamp: block.time,
            ..Default::default()
        };
        self.processor.sysvar_cache_mut().set_clock(&clock);
    }

    /// Slot the context is currently executing against.
    pub(crate) fn slot(&self) -> Slot {
        self.processor.slot
    }

    /// Derives the compute budget and limits from the transaction's compute-budget
    /// instructions. Fees are forced to zero and depth-8 CPIs disabled on the ER.
    pub(crate) fn parse_details(
        &self,
        txn: &ResolvedTransaction,
        features: &FeatureSet,
    ) -> TransactionResult<CheckedTransactionDetails> {
        let ixs = txn.program_instructions_iter();
        let limits = process_compute_budget_instructions(ixs, features)?;
        let mut limits = limits.get_compute_budget_and_limits(
            limits.loaded_accounts_bytes,
            Default::default(), // Fee is always zero on ER
            false,              // Depth-8 CPIs are disabled on solana
        );
        if matches!(txn.version(), TransactionVersion::Magicblock) {
            limits.budget.max_instruction_trace_length = MAGICBLOCK_INSTRUCTION_TRACE_LENGTH;
        }
        Ok(CheckedTransactionDetails::new(None, limits))
    }
}
