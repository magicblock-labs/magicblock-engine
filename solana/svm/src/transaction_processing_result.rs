use {
    crate::transaction_execution_result::ExecutedTransaction,
    solana_transaction_error::TransactionResult,
};

/// Result of loading and executing a transaction.
///
/// `Err` means execution did not produce an `ExecutedTransaction`. `Ok` means
/// execution was attempted; inspect the contained execution status for program
/// success or failure.
pub type TransactionProcessingResult = TransactionResult<Box<ExecutedTransaction>>;

/// Convenience methods for nested transaction processing results.
pub trait TransactionProcessingResultExtensions {
    /// Returns true when the transaction reached execution.
    fn was_processed(&self) -> bool;
    /// Returns true when the transaction reached execution and succeeded.
    fn was_processed_with_successful_result(&self) -> bool;
    /// Returns the executed transaction when execution was attempted.
    fn processed_transaction(&self) -> Option<&ExecutedTransaction>;
    /// Collapses load and execution status into a single transaction result.
    fn flattened_result(&self) -> TransactionResult<()>;
}

impl TransactionProcessingResultExtensions for TransactionProcessingResult {
    fn was_processed(&self) -> bool {
        self.is_ok()
    }

    fn was_processed_with_successful_result(&self) -> bool {
        match self {
            Ok(processed_tx) => processed_tx.was_successful(),
            Err(_) => false,
        }
    }

    fn processed_transaction(&self) -> Option<&ExecutedTransaction> {
        self.as_deref().ok()
    }

    fn flattened_result(&self) -> TransactionResult<()> {
        self.as_ref()
            .map_err(|err| err.clone())
            .and_then(|processed_tx| processed_tx.execution_details.status.clone())
    }
}
