//! Internal helpers shared by keeper accessors.

use ledger::{
    request::{ReadRequest, RequestPayload, TransactionStatus},
    schema::{
        AccountIndex, Balances, CompiledInstruction, Cpis, Event, Execution, ExecutionDetails,
        ExecutionHeader, Instruction, ReturnData,
    },
};
use nucleus::Slot;
use solana_message::inner_instruction::{InnerInstruction, InnerInstructionsList};
use solana_signature::Signature;
use solana_svm::{
    transaction_balances::BalanceCollector,
    transaction_execution_result::ExecutedTransaction,
    transaction_processing_result::{
        TransactionProcessingResult, TransactionProcessingResultExtensions,
    },
};

use crate::{FullTransaction, Keeper, Result};

/// Ledger event and status-cache entry derived from one execution result.
pub(crate) struct ExecutionCommit {
    /// First transaction signature used for status notifications.
    pub(crate) signature: Signature,
    /// Status stored in the signature cache and sent to subscribers.
    pub(crate) status: TransactionStatus,
    /// Ledger event that pairs execution metadata with the appended transaction.
    pub(crate) event: Event,
}

/// Sends a typed read request to the ledger reader and waits for its response.
pub(crate) async fn request<F, P, R>(keeper: &Keeper, params: P, request: F) -> Result<R>
where
    F: FnOnce(RequestPayload<P, R>) -> ReadRequest,
{
    let (payload, handle) = RequestPayload::<P, R>::new(params);
    let request = request(payload);
    keeper.ledger.reader.send_async(request).await?;
    Ok(handle.recv().await?)
}

/// Builds the ledger and cache records for a completed transaction execution.
/// Retains balances in the transaction only when live notifications need them.
pub(crate) fn execution_commit(
    txn: &mut FullTransaction,
    retain_balances: bool,
) -> ExecutionCommit {
    let slot = txn.execution.slot;
    let signature = txn.transaction.signatures()[0];
    let status = transaction_status(&txn.execution.result, slot);

    let header = ExecutionHeader {
        signature,
        slot,
        result: status.result.clone(),
    };
    let details = txn.execution.result.as_ref().ok().map(|execution| {
        let balances = if retain_balances {
            txn.execution.balances.clone()
        } else {
            txn.execution.balances.take()
        };
        execution_details(execution, balances)
    });
    let accounts = AccountIndex::new(txn.transaction.static_account_keys());
    let event = Event::Execution {
        execution: Execution { header, details },
        accounts,
    };

    ExecutionCommit { signature, status, event }
}

/// Projects an SVM result into the terminal status shared by live and replay commits.
pub(crate) fn transaction_status(
    result: &TransactionProcessingResult,
    slot: Slot,
) -> TransactionStatus {
    TransactionStatus {
        result: result.flattened_result(),
        slot,
    }
}

/// Projects SVM execution data into the retained ledger format.
fn execution_details(
    execution: &ExecutedTransaction,
    balances: Option<BalanceCollector>,
) -> ExecutionDetails {
    let (pre, mut post) = balances.map(|bc| bc.into_vecs()).unwrap_or_default();
    let details = &execution.execution_details;
    if details.status.is_err() {
        // Failed executions commit no account changes; fees are zero on the ER.
        post.clone_from(&pre);
    }

    ExecutionDetails {
        fee: execution.loaded_transaction.fee_details.total_fee(),
        balances: Balances { pre, post },
        logs: details.log_messages.clone().unwrap_or_default(),
        compute_units: details.executed_units,
        return_data: details.return_data.as_ref().map(|rd| ReturnData {
            program: rd.program_id.to_bytes(),
            data: rd.data.clone().into(),
        }),
        cpi: details.inner_instructions.as_ref().map(cpis),
    }
}

/// Projects grouped SVM inner instructions into ledger CPI records.
fn cpis(groups: &InnerInstructionsList) -> Vec<Cpis> {
    let iter = groups.iter();
    iter.map(|group| Cpis(group.iter().map(instruction).collect())).collect()
}

/// Projects one SVM inner instruction into the ledger instruction format.
fn instruction(ix: &InnerInstruction) -> Instruction {
    Instruction {
        stack_height: ix.stack_height,
        compiled: CompiledInstruction {
            program_index: ix.instruction.program_id_index,
            accounts: ix.instruction.accounts.clone(),
            data: ix.instruction.data.clone(),
        },
    }
}
