#[cfg(test)]
use solana_svm_type_overrides::sync::RwLock;
use solana_transaction_context::transaction_accounts::KeyedAccountSharedData;

#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::{field_qualifiers, qualifiers};
use {
    crate::{
        account_loader::{
            CheckedTransactionDetails, LoadedTransaction, TransactionLoadResult,
            ValidatedTransactionDetails, load_transaction,
        },
        message_processor::process_message,
        program_loader::load_program,
        transaction_account_state_info::TransactionAccountStateInfo,
        transaction_balances::{BalanceCollectionRoutines, BalanceCollector},
        transaction_execution_result::{ExecutedTransaction, TransactionExecutionDetails},
        transaction_processing_result::TransactionProcessingResult,
    },
    solana_account::{AccountSharedData, PROGRAM_OWNERS, ReadableAccount},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_instruction::TRANSACTION_LEVEL_STACK_HEIGHT,
    solana_message::{
        compiled_instruction::CompiledInstruction,
        inner_instruction::{InnerInstruction, InnerInstructionsList},
    },
    solana_program_runtime::{
        execution_budget::SVMTransactionExecutionCost,
        invoke_context::{EnvironmentConfig, InvokeContext},
        loaded_programs::{ProgramCache, ProgramCacheForTxBatch, ProgramRuntimeEnvironments},
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_svm_callback::TransactionProcessingCallback,
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_log_collector::LogCollector,
    solana_svm_transaction::svm_transaction::SVMTransaction,
    solana_svm_type_overrides::sync::Arc,
    solana_transaction_context::transaction::{ExecutionRecord, TransactionContext},
    solana_transaction_error::TransactionError,
    std::{
        fmt::{Debug, Formatter},
        rc::Rc,
    },
};

/// Log messages emitted during a transaction.
pub type TransactionLogMessages = Vec<String>;

/// Result of loading and executing one sanitized transaction.
pub struct LoadAndExecuteSanitizedTransactionOutput {
    /// Load or execution result.
    ///
    /// `Ok` means execution was attempted. The contained transaction can still
    /// have a failed execution status.
    pub processing_result: TransactionProcessingResult,
    /// Native pre/post balances when balance recording is enabled.
    pub balance_collector: Option<BalanceCollector>,
}

/// Controls which execution artifacts are retained in the result.
#[derive(Copy, Clone, Default)]
pub struct ExecutionRecordingConfig {
    /// Record inner instructions produced by CPI.
    pub enable_cpi_recording: bool,
    /// Record program log messages.
    pub enable_log_recording: bool,
    /// Record non-empty return data.
    pub enable_return_data_recording: bool,
    /// Record native account balances before and after execution.
    pub enable_transaction_balance_recording: bool,
}

impl ExecutionRecordingConfig {
    /// Creates a recording config with every flag set to the same value.
    pub fn new_single_setting(option: bool) -> Self {
        ExecutionRecordingConfig {
            enable_return_data_recording: option,
            enable_log_recording: option,
            enable_cpi_recording: option,
            enable_transaction_balance_recording: option,
        }
    }
}

/// Transaction execution options.
#[derive(Default)]
pub struct TransactionProcessingConfig {
    /// The maximum number of bytes that log messages can consume.
    pub log_messages_bytes_limit: Option<usize>,
    /// Recording capabilities for transaction execution.
    pub recording_config: ExecutionRecordingConfig,
}

/// Runtime inputs that are shared across a transaction execution.
#[derive(Default)]
pub struct TransactionProcessingEnvironment {
    /// Blockhash exposed to programs through the invocation environment.
    pub blockhash: Hash,
    /// Lamports per signature associated with `blockhash`.
    pub blockhash_lamports_per_signature: u64,
    /// Retained for API compatibility; the current execution path does not use
    /// stake weighting.
    pub epoch_total_stake: u64,
    /// Runtime feature set used during execution.
    pub feature_set: SVMFeatureSet,
    /// Runtime environments used for executing already deployed programs.
    pub program_runtime_environments_for_execution: ProgramRuntimeEnvironments,
    /// Rent calculator used for transaction-context construction and rent-state checks.
    pub rent: Rent,
}

#[cfg_attr(
    feature = "dev-context-only-utils",
    field_qualifiers(slot(pub), sysvar_cache(pub))
)]
pub struct TransactionBatchProcessor {
    /// Slot associated with this processor.
    pub slot: Slot,

    /// Sysvars exposed to programs during execution.
    sysvar_cache: SysvarCache,

    /// Shared cache of loaded programs.
    pub program_cache: Arc<ProgramCache>,

    execution_cost: SVMTransactionExecutionCost,
}

impl Debug for TransactionBatchProcessor {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransactionBatchProcessor")
            .field("slot", &self.slot)
            .field("sysvar_cache", &self.sysvar_cache)
            .field("program_cache", &self.program_cache)
            .finish()
    }
}

impl Default for TransactionBatchProcessor {
    fn default() -> Self {
        Self {
            slot: Slot::default(),
            sysvar_cache: Default::default(),
            program_cache: Arc::new(ProgramCache::default()),
            execution_cost: SVMTransactionExecutionCost::default(),
        }
    }
}

impl TransactionBatchProcessor {
    /// Create a `TransactionBatchProcessor` using the supplied program cache.
    ///
    /// The processor preserves the cache contents and does not add builtins.
    pub fn new_uninitialized(slot: Slot, cache: Arc<ProgramCache>) -> Self {
        Self {
            slot,
            program_cache: cache,
            ..Self::default()
        }
    }

    /// Create a new `TransactionBatchProcessor`.
    ///
    /// Runtime environments are supplied per execution through
    /// [`TransactionProcessingEnvironment::program_runtime_environments_for_execution`].
    pub fn new(slot: Slot, cache: Arc<ProgramCache>) -> Self {
        Self::new_uninitialized(slot, cache)
    }

    /// Sets the base execution cost charged by this processor.
    pub fn set_execution_cost(&mut self, cost: SVMTransactionExecutionCost) {
        self.execution_cost = cost;
    }

    /// Returns mutable access to cached sysvars for the current processor slot.
    pub fn sysvar_cache_mut(&mut self) -> &mut SysvarCache {
        &mut self.sysvar_cache
    }

    /// Loads accounts, prepares programs, and executes one sanitized transaction.
    pub fn load_and_execute_sanitized_transaction<CB: TransactionProcessingCallback>(
        &self,
        callbacks: &CB,
        tx: &impl SVMTransaction,
        details: CheckedTransactionDetails,
        environment: &TransactionProcessingEnvironment,
        config: &TransactionProcessingConfig,
    ) -> LoadAndExecuteSanitizedTransactionOutput {
        // Create the transaction balance collector if recording is enabled.
        let mut balance_collector = config
            .recording_config
            .enable_transaction_balance_recording
            .then(BalanceCollector::default);

        // Create the batch-local program cache.
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::new(self.slot);

        let details = ValidatedTransactionDetails {
            compute_budget: details.compute_budget_and_limits.budget,
            loaded_accounts_bytes_limit: details
                .compute_budget_and_limits
                .loaded_accounts_data_size_limit,
            fee_details: details.compute_budget_and_limits.fee_details,
        };
        let load_result = load_transaction(callbacks, tx, details);

        let processing_result = match load_result {
            TransactionLoadResult::NotLoaded(err) => Err(err),
            TransactionLoadResult::Loaded(loaded_transaction) => {
                balance_collector.collect_pre_balances(&loaded_transaction.accounts);
                self.replenish_program_cache(
                    &environment.program_runtime_environments_for_execution,
                    &mut program_cache_for_tx_batch,
                    &loaded_transaction.accounts,
                );

                let mut executed_tx = self.execute_loaded_transaction(
                    callbacks,
                    tx,
                    loaded_transaction,
                    &mut program_cache_for_tx_batch,
                    environment,
                    config,
                );
                balance_collector.collect_post_balances(&executed_tx.loaded_transaction.accounts);
                if executed_tx.access_is_valid(tx) {
                    let cache = program_cache_for_tx_batch.drain_modified_entries();
                    self.program_cache.merge(&cache);
                }
                Ok(Box::new(executed_tx))
            }
        };

        LoadAndExecuteSanitizedTransactionOutput {
            processing_result,
            balance_collector,
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn replenish_program_cache(
        &self,
        environments: &ProgramRuntimeEnvironments,
        program_cache_for_tx_batch: &mut ProgramCacheForTxBatch,
        accounts: &[KeyedAccountSharedData],
    ) {
        for (pubkey, acc) in accounts {
            if !(acc.executable() && PROGRAM_OWNERS.iter().any(|o| o == acc.owner())) {
                continue;
            }
            let entry = if let Some(entry) = self.program_cache.get(pubkey) {
                entry
            } else {
                let entry = load_program(environments, acc);
                self.program_cache.assign_program(*pubkey, entry.clone());
                entry
            };
            program_cache_for_tx_batch.replenish(*pubkey, entry);
        }
    }

    /// Executes a transaction using already loaded accounts.
    #[allow(clippy::too_many_arguments)]
    fn execute_loaded_transaction<CB: TransactionProcessingCallback>(
        &self,
        callback: &CB,
        tx: &impl SVMTransaction,
        mut loaded_transaction: LoadedTransaction,
        program_cache_for_tx_batch: &mut ProgramCacheForTxBatch,
        environment: &TransactionProcessingEnvironment,
        config: &TransactionProcessingConfig,
    ) -> ExecutedTransaction {
        let transaction_accounts = std::mem::take(&mut loaded_transaction.accounts);

        // Ensure the length of accounts matches the expected length from tx.account_keys().
        // This is a sanity check in case that someone starts adding some additional accounts
        // since this has been done before. See discussion in PR #4497 for details
        debug_assert!(transaction_accounts.len() == tx.account_keys().len());

        fn transaction_accounts_lamports_sum(
            accounts: &[(Pubkey, AccountSharedData)],
        ) -> Option<u128> {
            accounts.iter().try_fold(0u128, |sum, (_, account)| {
                sum.checked_add(u128::from(account.lamports()))
            })
        }

        let lamports_before_tx =
            transaction_accounts_lamports_sum(&transaction_accounts).unwrap_or(0);

        let compute_budget = loaded_transaction.compute_budget;

        let mut transaction_context = TransactionContext::new(
            transaction_accounts,
            environment.rent.clone(),
            compute_budget.max_instruction_stack_depth,
            compute_budget.max_instruction_trace_length,
            tx.num_instructions(),
        );

        let pre_account_state_info =
            TransactionAccountStateInfo::new(&transaction_context, tx, &environment.rent);

        let log_collector = if config.recording_config.enable_log_recording {
            match config.log_messages_bytes_limit {
                None => Some(LogCollector::new_ref()),
                Some(log_messages_bytes_limit) => Some(LogCollector::new_ref_with_limit(Some(
                    log_messages_bytes_limit,
                ))),
            }
        } else {
            None
        };

        let mut executed_units = 0u64;

        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            program_cache_for_tx_batch,
            EnvironmentConfig::new(
                environment.blockhash,
                environment.blockhash_lamports_per_signature,
                callback,
                &environment.feature_set,
                &environment.program_runtime_environments_for_execution,
                &self.sysvar_cache,
            ),
            log_collector.clone(),
            compute_budget,
            self.execution_cost,
        );

        let process_result = process_message(
            tx,
            &loaded_transaction.program_indices,
            &mut invoke_context,
            &mut executed_units,
        );

        drop(invoke_context);

        let mut status = process_result.and_then(|info| {
            let post_account_state_info =
                TransactionAccountStateInfo::new(&transaction_context, tx, &environment.rent);
            TransactionAccountStateInfo::verify_changes(
                &pre_account_state_info,
                &post_account_state_info,
                &transaction_context,
            )
            .map(|_| info)
        });

        let log_messages: Option<TransactionLogMessages> =
            log_collector.and_then(|log_collector| {
                Rc::try_unwrap(log_collector)
                    .map(|log_collector| log_collector.into_inner().into_messages())
                    .ok()
            });

        let (execution_record, inner_instructions) = Self::deconstruct_transaction(
            transaction_context,
            config.recording_config.enable_cpi_recording,
        );

        let ExecutionRecord {
            accounts,
            return_data,
            accounts_resize_delta: accounts_data_len_delta,
            ..
        } = execution_record;

        if status.is_ok()
            && transaction_accounts_lamports_sum(&accounts)
                .filter(|lamports_after_tx| lamports_before_tx == *lamports_after_tx)
                .is_none()
        {
            status = Err(TransactionError::UnbalancedTransaction);
        }
        let status = status.map(|_| ());

        loaded_transaction.accounts = accounts;

        let return_data = if config.recording_config.enable_return_data_recording
            && !return_data.data.is_empty()
        {
            Some(return_data)
        } else {
            None
        };

        ExecutedTransaction {
            execution_details: TransactionExecutionDetails {
                status,
                log_messages: log_messages.map(Arc::new),
                inner_instructions,
                return_data,
                executed_units,
                accounts_data_len_delta,
            },
            loaded_transaction,
        }
    }

    /// Extract an ExecutionRecord and an InnerInstructionsList from a TransactionContext
    fn deconstruct_transaction(
        mut transaction_context: TransactionContext,
        record_inner_instructions: bool,
    ) -> (ExecutionRecord, Option<InnerInstructionsList>) {
        let inner_ix = if record_inner_instructions {
            debug_assert!(
                transaction_context
                    .get_instruction_context_at_index_in_trace(0)
                    .map(|instruction_context| instruction_context.get_stack_height()
                        == TRANSACTION_LEVEL_STACK_HEIGHT)
                    .unwrap_or(true)
            );

            let (ix_trace, accounts, ix_data_trace) = transaction_context.take_instruction_trace();
            let mut outer_instructions = Vec::new();
            for ((ix_in_trace, ix_data), ix_accounts) in
                ix_trace.into_iter().zip(ix_data_trace).zip(accounts)
            {
                let stack_height = ix_in_trace.nesting_level.saturating_add(1) as usize;
                if stack_height == TRANSACTION_LEVEL_STACK_HEIGHT {
                    outer_instructions.push(Vec::new());
                } else if let Some(inner_instructions) = outer_instructions.last_mut() {
                    let stack_height = u8::try_from(stack_height).unwrap_or(u8::MAX);
                    inner_instructions.push(InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(
                            ix_in_trace.program_account_index_in_tx as u8,
                            ix_data.into_owned(),
                            ix_accounts.iter().map(|acc| acc.index_in_transaction as u8).collect(),
                        ),
                        stack_height,
                    });
                } else {
                    debug_assert!(false);
                }
            }

            Some(outer_instructions)
        } else {
            None
        };

        let record: ExecutionRecord = transaction_context.into();

        (record, inner_ix)
    }

    pub fn fill_missing_sysvar_cache_entries<CB: TransactionProcessingCallback>(
        &mut self,
        callbacks: &CB,
    ) {
        self.sysvar_cache.fill_missing_entries(|pubkey, set_sysvar| {
            if let Some((account, _slot)) = callbacks.get_account_shared_data(pubkey) {
                set_sysvar(account.data());
            }
        });
    }

    pub fn reset_sysvar_cache(&mut self) {
        self.sysvar_cache.reset();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    #[allow(deprecated)]
    use solana_sysvar::fees::Fees;
    use {
        super::*,
        solana_account::{WritableAccount, create_account_shared_data_for_test},
        solana_clock::Clock,
        solana_epoch_schedule::EpochSchedule,
        solana_fee_calculator::FeeCalculator,
        solana_fee_structure::FeeDetails,
        solana_hash::Hash,
        solana_message::{LegacyMessage, Message, MessageHeader, SanitizedMessage},
        solana_program_runtime::{
            execution_budget::SVMTransactionExecutionBudget, loaded_programs::ProgramCacheEntryType,
        },
        solana_rent::Rent,
        solana_sdk_ids::{bpf_loader, sysvar},
        solana_signature::Signature,
        solana_svm_callback::{AccountState, InvokeContextCallback},
        solana_transaction::sanitized::SanitizedTransaction,
        solana_transaction_context::transaction::TransactionContext,
        std::collections::HashMap,
    };

    fn new_unchecked_sanitized_message(message: Message) -> SanitizedMessage {
        SanitizedMessage::Legacy(LegacyMessage::new(message, &HashSet::new()))
    }

    #[derive(Clone, Default)]
    struct MockBankCallback {
        account_shared_data: Arc<RwLock<HashMap<Pubkey, AccountSharedData>>>,
        #[allow(clippy::type_complexity)]
        inspected_accounts:
            Arc<RwLock<HashMap<Pubkey, Vec<(Option<AccountSharedData>, /* is_writable */ bool)>>>>,
    }

    impl InvokeContextCallback for MockBankCallback {}

    impl TransactionProcessingCallback for MockBankCallback {
        fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
            self.account_shared_data
                .read()
                .unwrap()
                .get(pubkey)
                .map(|account| (account.clone(), 0))
        }

        fn inspect_account(
            &self,
            address: &Pubkey,
            account_state: AccountState,
            is_writable: bool,
        ) {
            let account = match account_state {
                AccountState::Dead => None,
                AccountState::Alive(account) => Some(account.clone()),
            };
            self.inspected_accounts
                .write()
                .unwrap()
                .entry(*address)
                .or_default()
                .push((account, is_writable));
        }
    }

    #[test]
    fn test_inner_instructions_list_from_instruction_trace() {
        let mut transaction_context = TransactionContext::new(
            vec![(
                Pubkey::new_unique(),
                AccountSharedData::new(1, 1, &bpf_loader::ID),
            )],
            Rent::default(),
            4,
            11,
            4,
        );

        // To be uncommented when we reorder the instruction trace
        // Four top level instructions
        // for i in 0..4 {
        //     transaction_context
        //         .configure_instruction_at_index(
        //             i,
        //             0,
        //             vec![],
        //             vec![u16::MAX; 256],
        //             Cow::Owned(vec![i as u8]),
        //             None,
        //         )
        //         .unwrap();
        // }

        // Execute ix #0
        transaction_context
            .configure_top_level_instruction_for_tests(0, vec![], vec![0])
            .unwrap();
        transaction_context.push().unwrap();
        // ix #0 does a CPI
        transaction_context.configure_next_cpi_for_tests(0, vec![], vec![0, 0]).unwrap();
        transaction_context.push().unwrap();
        // Returning from everything
        transaction_context.pop().unwrap();
        transaction_context.pop().unwrap();
        // Execute ix #1
        transaction_context
            .configure_top_level_instruction_for_tests(0, vec![], vec![1])
            .unwrap();
        transaction_context.push().unwrap();
        transaction_context.pop().unwrap();
        // Execute ix #2
        transaction_context
            .configure_top_level_instruction_for_tests(0, vec![], vec![2])
            .unwrap();
        transaction_context.push().unwrap();
        // ix #2 does a CPI
        transaction_context.configure_next_cpi_for_tests(0, vec![], vec![2, 0]).unwrap();
        transaction_context.push().unwrap();
        // A nested CPI
        transaction_context.configure_next_cpi_for_tests(0, vec![], vec![2, 1]).unwrap();
        transaction_context.push().unwrap();
        // Return from nested CPI
        transaction_context.pop().unwrap();
        // Return from CPI
        transaction_context.pop().unwrap();
        // ix #2 does another CPI
        transaction_context.configure_next_cpi_for_tests(0, vec![], vec![2, 2]).unwrap();
        transaction_context.push().unwrap();
        // Return from everything related to ix #2
        transaction_context.pop().unwrap();
        transaction_context.pop().unwrap();
        // Execute ix #3
        transaction_context
            .configure_top_level_instruction_for_tests(0, vec![], vec![3])
            .unwrap();
        transaction_context.push().unwrap();
        // ix #3 does a CPI
        transaction_context.configure_next_cpi_for_tests(0, vec![], vec![3, 0]).unwrap();
        transaction_context.push().unwrap();
        // ix #3 does a nested CPI
        transaction_context.configure_next_cpi_for_tests(0, vec![], vec![3, 1]).unwrap();
        transaction_context.push().unwrap();
        // ix #3 does a second nested CPI
        transaction_context.configure_next_cpi_for_tests(0, vec![], vec![3, 2]).unwrap();
        transaction_context.push().unwrap();
        // Return from everything related to ix #3
        transaction_context.pop().unwrap();
        transaction_context.pop().unwrap();
        transaction_context.pop().unwrap();
        transaction_context.pop().unwrap();

        let inner_instructions =
            TransactionBatchProcessor::deconstruct_transaction(transaction_context, true)
                .1
                .unwrap();

        assert_eq!(
            inner_instructions,
            vec![
                vec![InnerInstruction {
                    instruction: CompiledInstruction::new_from_raw_parts(0, vec![0, 0], vec![]),
                    stack_height: 2,
                }],
                vec![],
                vec![
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![2, 0], vec![]),
                        stack_height: 2,
                    },
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![2, 1], vec![]),
                        stack_height: 3,
                    },
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![2, 2], vec![]),
                        stack_height: 2,
                    },
                ],
                vec![
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![3, 0], vec![]),
                        stack_height: 2,
                    },
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![3, 1], vec![]),
                        stack_height: 3,
                    },
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![3, 2], vec![]),
                        stack_height: 4,
                    },
                ]
            ]
        );
    }

    #[test]
    fn test_execute_loaded_transaction_recordings() {
        // Setting all the arguments correctly is too burdensome for testing
        // execute_loaded_transaction separately.This function will be tested in an integration
        // test with load_and_execute_sanitized_transactions
        let message = Message {
            account_keys: vec![Pubkey::new_from_array([0; 32])],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 0,
                accounts: vec![],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::default();
        let batch_processor = TransactionBatchProcessor::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let loaded_transaction = LoadedTransaction {
            accounts: vec![(Pubkey::new_unique(), AccountSharedData::default())],
            program_indices: vec![0],
            fee_details: FeeDetails::default(),
            compute_budget: SVMTransactionExecutionBudget::default(),
            loaded_accounts_data_size: 32,
        };

        let processing_environment = TransactionProcessingEnvironment::default();

        let mut processing_config = TransactionProcessingConfig::default();
        processing_config.recording_config.enable_log_recording = true;

        let mock_bank = MockBankCallback::default();

        let executed_tx = batch_processor.execute_loaded_transaction(
            &mock_bank,
            &sanitized_transaction,
            loaded_transaction.clone(),
            &mut program_cache_for_tx_batch,
            &processing_environment,
            &processing_config,
        );
        assert!(executed_tx.execution_details.log_messages.is_some());

        processing_config.log_messages_bytes_limit = Some(2);

        let executed_tx = batch_processor.execute_loaded_transaction(
            &mock_bank,
            &sanitized_transaction,
            loaded_transaction.clone(),
            &mut program_cache_for_tx_batch,
            &processing_environment,
            &processing_config,
        );
        assert!(executed_tx.execution_details.log_messages.is_some());
        assert!(executed_tx.execution_details.inner_instructions.is_none());

        processing_config.recording_config.enable_log_recording = false;
        processing_config.recording_config.enable_cpi_recording = true;
        processing_config.log_messages_bytes_limit = None;

        let executed_tx = batch_processor.execute_loaded_transaction(
            &mock_bank,
            &sanitized_transaction,
            loaded_transaction,
            &mut program_cache_for_tx_batch,
            &processing_environment,
            &processing_config,
        );

        assert!(executed_tx.execution_details.log_messages.is_none());
        assert!(executed_tx.execution_details.inner_instructions.is_some());
    }

    #[test]
    fn test_replenish_program_cache() {
        let batch_processor = TransactionBatchProcessor::default();
        let key = Pubkey::new_unique();

        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader::id());
        account_data.set_executable(true);
        let accounts = vec![(key, account_data)];

        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::new(batch_processor.slot);

        batch_processor.replenish_program_cache(
            &Default::default(),
            &mut program_cache_for_tx_batch,
            &accounts,
        );

        let program = program_cache_for_tx_batch.find(&key).unwrap();
        assert!(matches!(
            program.program,
            ProgramCacheEntryType::FailedVerification(_)
        ));
        assert!(batch_processor.program_cache.get(&key).is_some());
    }

    #[test]
    #[allow(deprecated)]
    fn test_sysvar_cache_initialization1() {
        let mock_bank = MockBankCallback::default();

        let clock = Clock {
            slot: 1,
            epoch_start_timestamp: 2,
            epoch: 3,
            leader_schedule_epoch: 4,
            unix_timestamp: 5,
        };
        let clock_account = create_account_shared_data_for_test(&clock);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::clock::id(), clock_account);

        let epoch_schedule = EpochSchedule::custom(64, 2, true);
        let epoch_schedule_account = create_account_shared_data_for_test(&epoch_schedule);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::epoch_schedule::id(), epoch_schedule_account);

        let fees = Fees {
            fee_calculator: FeeCalculator { lamports_per_signature: 123 },
        };
        let fees_account = create_account_shared_data_for_test(&fees);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::fees::id(), fees_account);

        let rent = Rent::default();
        let rent_account = create_account_shared_data_for_test(&rent);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::rent::id(), rent_account);

        let mut transaction_processor = TransactionBatchProcessor::default();
        transaction_processor.fill_missing_sysvar_cache_entries(&mock_bank);

        let sysvar_cache = &transaction_processor.sysvar_cache;
        let cached_clock = sysvar_cache.get_clock();
        let cached_rent = sysvar_cache.get_rent();

        assert_eq!(
            cached_clock.expect("clock sysvar missing in cache"),
            clock.into()
        );
        assert_eq!(
            cached_rent.expect("rent sysvar missing in cache"),
            rent.into()
        );
        assert!(sysvar_cache.get_slot_hashes().is_err());
    }

    #[test]
    #[allow(deprecated)]
    fn test_reset_and_fill_sysvar_cache() {
        let mock_bank = MockBankCallback::default();

        let clock = Clock {
            slot: 1,
            epoch_start_timestamp: 2,
            epoch: 3,
            leader_schedule_epoch: 4,
            unix_timestamp: 5,
        };
        let clock_account = create_account_shared_data_for_test(&clock);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::clock::id(), clock_account);

        let epoch_schedule = EpochSchedule::custom(64, 2, true);
        let epoch_schedule_account = create_account_shared_data_for_test(&epoch_schedule);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::epoch_schedule::id(), epoch_schedule_account);

        let fees = Fees {
            fee_calculator: FeeCalculator { lamports_per_signature: 123 },
        };
        let fees_account = create_account_shared_data_for_test(&fees);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::fees::id(), fees_account);

        let rent = Rent::default();
        let rent_account = create_account_shared_data_for_test(&rent);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::rent::id(), rent_account);

        let mut transaction_processor = TransactionBatchProcessor::default();
        // Fill the sysvar cache
        transaction_processor.fill_missing_sysvar_cache_entries(&mock_bank);
        // Reset the sysvar cache
        transaction_processor.reset_sysvar_cache();

        {
            let sysvar_cache = &transaction_processor.sysvar_cache;
            // Test that sysvar cache is empty and none of the values are found
            assert!(sysvar_cache.get_clock().is_err());
            assert!(sysvar_cache.get_rent().is_err());
        }

        // Refill the cache and test the values are available.
        transaction_processor.fill_missing_sysvar_cache_entries(&mock_bank);

        let sysvar_cache = &transaction_processor.sysvar_cache;
        let cached_clock = sysvar_cache.get_clock();
        let cached_rent = sysvar_cache.get_rent();

        assert_eq!(
            cached_clock.expect("clock sysvar missing in cache"),
            clock.into()
        );
        assert_eq!(
            cached_rent.expect("rent sysvar missing in cache"),
            rent.into()
        );
        assert!(sysvar_cache.get_slot_hashes().is_err());
    }
}
