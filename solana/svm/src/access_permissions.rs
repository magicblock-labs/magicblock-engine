use solana_account::{AccountMode, AccountSharedData, DirtyMarkers};
use solana_svm_transaction::svm_message::SVMMessage;
use solana_transaction_error::TransactionError;
use std::sync::Arc;

use crate::transaction_execution_result::ExecutedTransaction;

impl ExecutedTransaction {
    /// Enforces engine account mutability after successful execution.
    pub(crate) fn access_is_valid(&mut self, tx: &impl SVMMessage) -> bool {
        if !self.was_successful() {
            return false;
        }
        let privileged = is_privileged(tx);
        let mut accounts = self.loaded_transaction.accounts.iter().enumerate();
        let Some((_, payer)) = accounts.next() else {
            // Sanitized transactions always carry a fee payer.
            return false;
        };
        let logs = Arc::make_mut(self.execution_details.log_messages.get_or_insert_default());
        for (i, (pk, acc)) in accounts {
            if !tx.is_writable(i) || writable(acc) || privileged {
                continue;
            }
            let error = format!("Program log: Immutable account {pk} has been modified");
            logs.push(error);
            self.execution_details.status = Err(TransactionError::InvalidWritableAccount);
            return false;
        }
        // The payer is writable in Solana messages even when fees are disabled
        // here; reject it only if execution actually changed immutable state.
        if payer.1.dirty() && !writable(&payer.1) {
            let error = format!("Program log: ({}) Feepayer account is readonly", payer.0);
            logs.push(error);
            self.execution_details.status = Err(TransactionError::InvalidAccountForFee);
            return false;
        }
        true
    }
}

/// Returns whether an unprivileged transaction may leave the account modified.
///
/// Mutable modes are writable directly. `Transient` and `Closed` are writable
/// only when the mode dirty marker records a lifecycle transition in the
/// current transaction.
fn writable(account: &AccountSharedData) -> bool {
    if account.mutable() {
        return true;
    }
    matches!(account.mode(), AccountMode::Transient | AccountMode::Closed)
        && account.markers().contains(DirtyMarkers::MODE)
}

/// Returns true when every instruction is handled by the MagicRoot authority path.
fn is_privileged(tx: &impl SVMMessage) -> bool {
    tx.program_instructions_iter().all(|(id, _)| *id == magic_root_interface::ID)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            account_loader::LoadedTransaction,
            transaction_execution_result::{ExecutedTransaction, TransactionExecutionDetails},
        },
        solana_account::{AccountBuilder, AccountMode, AccountSharedData},
        solana_hash::Hash,
        solana_message::{
            LegacyMessage, Message, MessageHeader, SanitizedMessage,
            compiled_instruction::CompiledInstruction,
        },
        solana_pubkey::Pubkey,
        solana_signature::Signature,
        solana_transaction::sanitized::SanitizedTransaction,
        solana_transaction_error::TransactionResult,
        std::collections::HashSet,
    };

    /// A dirtied account in the requested mode.
    fn account(mode: AccountMode) -> AccountSharedData {
        let mut acc: AccountSharedData = AccountBuilder::default().mode(mode).build();
        acc.set_data_from_slice(&[1]);
        acc
    }

    /// Builds a sanitized transaction over `account_keys` whose only writable
    /// non-signer accounts are the first `writable_non_signers` after the payer,
    /// invoking one instruction per entry in `program_indices`.
    ///
    /// Layout is `[payer, non-signers.., program..]`: the payer signs and is
    /// writable, and `is_writable(i)` follows directly from the header math the
    /// engine guard relies on.
    fn sanitized_tx(
        account_keys: Vec<Pubkey>,
        writable_non_signers: u8,
        program_indices: &[u8],
    ) -> SanitizedTransaction {
        let non_signers = account_keys.len() as u8 - 1;
        let header = MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: non_signers - writable_non_signers,
        };
        let instructions = program_indices
            .iter()
            .map(|&program_id_index| CompiledInstruction {
                program_id_index,
                accounts: vec![],
                data: vec![],
            })
            .collect();
        let message = Message {
            account_keys,
            header,
            instructions,
            recent_blockhash: Hash::default(),
        };
        let sanitized = SanitizedMessage::Legacy(LegacyMessage::new(message, &HashSet::new()));
        SanitizedTransaction::new_for_tests(sanitized, vec![Signature::new_unique()], false)
    }

    /// Wraps executed account state and a status into an `ExecutedTransaction`.
    fn executed(
        accounts: Vec<(Pubkey, AccountSharedData)>,
        status: TransactionResult<()>,
    ) -> ExecutedTransaction {
        ExecutedTransaction {
            loaded_transaction: LoadedTransaction { accounts, ..Default::default() },
            execution_details: TransactionExecutionDetails {
                status,
                log_messages: None,
                inner_instructions: None,
                return_data: None,
                executed_units: 0,
                accounts_data_len_delta: 0,
            },
        }
    }

    /// Runs the guard over a `[payer, target, program]` transaction, returning
    /// its verdict and the (possibly rewritten) execution details.
    ///
    /// `program` is MagicRoot when `privileged`, so the whole tx takes the
    /// authority path; only `target` (index 1) is ever writable in the message.
    fn run(
        payer: AccountSharedData,
        target: AccountSharedData,
        writable_target: bool,
        privileged: bool,
        status: TransactionResult<()>,
    ) -> (bool, ExecutedTransaction) {
        let payer_key = Pubkey::new_unique();
        let target_key = Pubkey::new_unique();
        let program = if privileged { magic_root_interface::ID } else { Pubkey::new_unique() };
        let tx = sanitized_tx(
            vec![payer_key, target_key, program],
            writable_target as u8,
            &[2],
        );
        let mut executed = executed(
            vec![
                (payer_key, payer),
                (target_key, target),
                (program, AccountSharedData::default()),
            ],
            status,
        );
        let verdict = executed.access_is_valid(&tx);
        (verdict, executed)
    }

    /// Asserts a log line containing `needle` was recorded.
    fn assert_logged(tx: &ExecutedTransaction, needle: &str) {
        let logs: &[String] = tx
            .execution_details
            .log_messages
            .as_deref()
            .map(Vec::as_slice)
            .unwrap_or_default();
        assert!(
            logs.iter().any(|l| l.contains(needle)),
            "expected {needle:?} in {logs:?}"
        );
    }

    #[test]
    fn writable_account_guard() {
        let mut transitioned = account(AccountMode::Delegated);
        transitioned.set_mode(AccountMode::Transient).unwrap();
        let mut closed = account(AccountMode::Ephemeral);
        closed.set_mode(AccountMode::Closed).unwrap();

        // A dirty, writable, immutable operand is rejected — unless the tx is
        // privileged or it legally entered a transaction-final mode. Mutable or
        // message-read-only operands are always fine.
        let cases = [
            // (target, writable, privileged, accepted)
            (account(AccountMode::ReadOnly), true, false, false),
            (account(AccountMode::Transient), true, false, false),
            (transitioned, true, false, true),
            (closed, true, false, true),
            (account(AccountMode::Delegated), true, false, true),
            (account(AccountMode::ReadOnly), false, false, true), // read-only in the message
            (account(AccountMode::ReadOnly), true, true, true),   // MagicRoot bypass
        ];
        for (i, (target, writable, privileged, accepted)) in cases.into_iter().enumerate() {
            let (verdict, executed) = run(
                AccountSharedData::default(),
                target,
                writable,
                privileged,
                Ok(()),
            );
            assert_eq!(verdict, accepted, "case {i}");
            let expected =
                if accepted { Ok(()) } else { Err(TransactionError::InvalidWritableAccount) };
            assert_eq!(executed.execution_details.status, expected, "case {i}");
            if !accepted {
                assert_logged(&executed, "Immutable account");
            }
        }
    }

    #[test]
    fn fee_payer_guard() {
        // A dirty immutable fee payer is rejected; a mutable
        // payer is always accepted.
        let cases = [
            // (payer, privileged, accepted)
            (account(AccountMode::ReadOnly), false, false),
            (account(AccountMode::Transient), false, false),
            (account(AccountMode::Delegated), false, true),
        ];
        for (i, (payer, privileged, accepted)) in cases.into_iter().enumerate() {
            let (verdict, executed) = run(
                payer,
                AccountSharedData::default(),
                false,
                privileged,
                Ok(()),
            );
            assert_eq!(verdict, accepted, "case {i}");
            let expected =
                if accepted { Ok(()) } else { Err(TransactionError::InvalidAccountForFee) };
            assert_eq!(executed.execution_details.status, expected, "case {i}");
            if !accepted {
                assert_logged(&executed, "Feepayer account");
            }
        }
    }

    #[test]
    fn guard_edge_cases() {
        // A failed run committed nothing, so a dirty immutable account must not
        // rewrite the original error into an access error.
        let (verdict, failed) = run(
            AccountSharedData::default(),
            account(AccountMode::ReadOnly),
            true,
            false,
            Err(TransactionError::AccountInUse),
        );
        assert!(!verdict);
        assert_eq!(
            failed.execution_details.status,
            Err(TransactionError::AccountInUse)
        );

        // A transaction without a fee payer account cannot be validated.
        let tx = sanitized_tx(vec![Pubkey::new_unique(), Pubkey::new_unique()], 0, &[1]);
        let mut payerless = executed(vec![], Ok(()));
        assert!(!payerless.access_is_valid(&tx));

        // Privilege requires *every* instruction to invoke MagicRoot.
        let keys = vec![Pubkey::new_unique(), magic_root_interface::ID, Pubkey::new_unique()];
        assert!(is_privileged(&sanitized_tx(keys.clone(), 0, &[1, 1])));
        assert!(!is_privileged(&sanitized_tx(keys, 0, &[1, 2])));
    }
}
