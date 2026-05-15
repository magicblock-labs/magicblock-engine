use {
    crate::account_loader::LoadedTransaction,
    solana_message::inner_instruction::InnerInstructionsList,
    solana_transaction_context::transaction::TransactionReturnData,
    solana_transaction_error::TransactionResult, std::sync::Arc,
};

/// Loaded-account statistics for one transaction.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TransactionLoadedAccountsStats {
    /// Total loaded account data size charged to the transaction.
    pub loaded_accounts_data_size: u32,
    /// Number of loaded transaction accounts.
    pub loaded_accounts_count: usize,
}

/// A transaction that reached execution, including mutated accounts.
#[derive(Debug, Clone)]
pub struct ExecutedTransaction {
    /// Loaded transaction state after execution.
    pub loaded_transaction: LoadedTransaction,
    /// Execution status and optional recording data.
    pub execution_details: TransactionExecutionDetails,
}

impl ExecutedTransaction {
    /// Returns true when execution completed with `Ok(())`.
    pub fn was_successful(&self) -> bool {
        self.execution_details.was_successful()
    }
}

/// Status and recordings produced by transaction execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionExecutionDetails {
    /// Final execution status.
    pub status: TransactionResult<()>,
    /// Program log messages when log recording is enabled.
    pub log_messages: Option<Arc<Vec<String>>>,
    /// CPI inner instructions when CPI recording is enabled.
    pub inner_instructions: Option<InnerInstructionsList>,
    /// Non-empty return data when return-data recording is enabled.
    pub return_data: Option<TransactionReturnData>,
    /// Compute units consumed by top-level instruction execution.
    pub executed_units: u64,
    /// The change in accounts data len for this transaction.
    /// NOTE: This value is valid IFF `status` is `Ok`.
    pub accounts_data_len_delta: i64,
}

impl TransactionExecutionDetails {
    /// Returns true when `status` is `Ok(())`.
    pub fn was_successful(&self) -> bool {
        self.status.is_ok()
    }
}
