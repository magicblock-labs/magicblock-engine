//! Shared SVM execution context for the transaction executor and simulator.

use std::sync::Arc;

use accountsdb::AccountLoader;
use agave_feature_set::FeatureSet;
use agave_transaction_view::{
    MAGICBLOCK_INSTRUCTION_TRACE_LENGTH, transaction_version::TransactionVersion,
};
use keeper::{Keeper, ResolvedTransaction};
use nucleus::Slot;
use solana_compute_budget_instruction::instructions_processor::process_compute_budget_instructions;
use solana_hash::Hash;
use solana_program_runtime::{
    execution_budget::{
        MAX_COMPUTE_UNIT_LIMIT, MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES, MIN_HEAP_FRAME_BYTES,
        SVMTransactionExecutionAndFeeBudgetLimits, SVMTransactionExecutionBudget,
    },
    loaded_programs::{ProgramCache, ProgramRuntimeEnvironments},
};
use solana_svm::{
    account_loader::CheckedTransactionDetails,
    transaction_processor::{
        ExecutionRecordingConfig, LoadAndExecuteSanitizedTransactionOutput,
        TransactionBatchProcessor, TransactionProcessingConfig, TransactionProcessingEnvironment,
    },
};
use solana_sysvar::clock::Clock;
use solana_transaction_error::TransactionResult;

use crate::{
    Result,
    callback::{InvokeCallback, LoadCallback},
};

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
            false, // Accept legacy ELFs already deployed on the base chain.
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
        let mut processor = TransactionBatchProcessor::new(state.clock(block).slot, cache);
        let accessor = state.accounts();
        let callback = LoadCallback::<false> { loader: accessor.loader() };
        processor.fill_missing_sysvar_cache_entries(&callback);
        let config = TransactionProcessingConfig {
            log_messages_bytes_limit: None,
            recording_config: ExecutionRecordingConfig::new_single_setting(true),
        };
        Ok(Self { processor, env, config })
    }

    /// Loads and executes a single transaction through the SVM.
    pub(crate) fn execute<const LOAD_OWNED: bool>(
        &self,
        loader: AccountLoader<'_>,
        txn: &ResolvedTransaction,
        features: &FeatureSet,
    ) -> LoadAndExecuteSanitizedTransactionOutput {
        let details = match Self::parse_details(txn, features) {
            Ok(d) => d,
            Err(e) => {
                return LoadAndExecuteSanitizedTransactionOutput {
                    processing_result: Err(e),
                    balance_collector: None,
                };
            }
        };
        let load = LoadCallback::<LOAD_OWNED> { loader };
        let invoke = InvokeCallback { featureset: features };
        self.processor.load_and_execute_sanitized_transaction(
            load,
            &invoke,
            txn,
            details,
            &self.env,
            &self.config,
        )
    }

    /// Advances the context to a new block: bumps the slot and blockhash and
    /// refreshes the cached clock sysvar.
    pub(crate) fn transition(&mut self, blockhash: Hash, clock: Clock) {
        self.processor.slot = clock.slot;
        self.env.blockhash = blockhash;
        self.processor.sysvar_cache_mut().set_clock(&clock);
    }

    /// Slot the context is currently executing against.
    pub(crate) fn slot(&self) -> Slot {
        self.processor.slot
    }

    /// Derives limits from V1 inline config or compute-budget instructions for
    /// other versions. Fees are zero and depth-8 CPIs disabled on the ER.
    fn parse_details(
        txn: &ResolvedTransaction,
        features: &FeatureSet,
    ) -> TransactionResult<CheckedTransactionDetails> {
        // Magicblock shares V1 framing but retains instruction-derived limits.
        let (units, heap, data_limit) = if let Some(config) = txn.transaction_config()
            && matches!(txn.version(), TransactionVersion::V1)
        {
            // Sanitization already validates heap size. Unlike legacy/V0, absent
            // V1 compute and loaded-data limits mean zero, not runtime defaults.
            let data_limit = config.loaded_accounts_data_size_limit().unwrap_or(0);
            (
                config.compute_unit_limit().unwrap_or(0).min(MAX_COMPUTE_UNIT_LIMIT),
                config.requested_heap_size().unwrap_or(MIN_HEAP_FRAME_BYTES),
                data_limit.min(MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get()),
            )
        } else {
            let limits =
                process_compute_budget_instructions(txn.program_instructions_iter(), features)?;
            (
                limits.compute_unit_limit,
                limits.updated_heap_bytes,
                limits.loaded_accounts_bytes.get(),
            )
        };
        let mut budget = SVMTransactionExecutionBudget {
            compute_unit_limit: u64::from(units),
            heap_size: heap,
            ..SVMTransactionExecutionBudget::default() // Depth-8 CPIs remain disabled.
        };
        if matches!(txn.version(), TransactionVersion::Magicblock) {
            budget.max_instruction_trace_length = MAGICBLOCK_INSTRUCTION_TRACE_LENGTH;
        }
        let limits = SVMTransactionExecutionAndFeeBudgetLimits {
            budget,
            loaded_accounts_data_size_limit: data_limit,
            fee_details: Default::default(), // Fees are always zero on ER.
        };
        Ok(CheckedTransactionDetails::new(None, limits))
    }
}
