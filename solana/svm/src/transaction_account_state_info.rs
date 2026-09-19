use {
    crate::rent_calculator::{RentState, check_rent_state, get_account_rent_state},
    solana_account::ReadableAccount,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction_context::{IndexOfAccount, transaction::TransactionContext},
    solana_transaction_error::TransactionResult as Result,
};

#[derive(Default, PartialEq, Debug)]
pub(crate) struct TransactionAccountStateInfo {
    rent_state: Option<RentState>, // None: readonly account
    balance: u64,
    data_size: usize,
    owner: Pubkey,
}

impl TransactionAccountStateInfo {
    pub(crate) fn new(
        transaction_context: &TransactionContext,
        message: &impl SVMMessage,
        rent: &Rent,
        relax_post_exec_min_balance_check: bool,
    ) -> Vec<Self> {
        (0..message.account_keys().len())
            .map(|i| {
                if !message.is_writable(i) {
                    return Self::default();
                }
                let state = transaction_context
                    .accounts()
                    .try_borrow(i as IndexOfAccount)
                    .map(|acc| {
                        let mut rent_state = get_account_rent_state(rent, &acc);
                        // SIMD-0392 treats existing funded accounts as rent-exempt.
                        if relax_post_exec_min_balance_check
                            && matches!(rent_state, RentState::RentPaying { .. })
                        {
                            rent_state = RentState::RentExempt;
                        }
                        Self {
                            rent_state: Some(rent_state),
                            balance: acc.lamports(),
                            data_size: acc.data().len(),
                            owner: *acc.owner(),
                        }
                    })
                    .ok();
                debug_assert!(
                    state.is_some(),
                    "message and transaction context out of sync, fatal"
                );
                state.unwrap_or_default()
            })
            .collect()
    }

    pub(crate) fn new_post_exec(
        transaction_context: &TransactionContext,
        message: &impl SVMMessage,
        rent: &Rent,
        pre_state_infos: &[Self],
        relax_post_exec_min_balance_check: bool,
    ) -> Vec<Self> {
        // Start with the normal classification, including Engine's Magic exemption.
        let mut post_state_infos = Self::new(transaction_context, message, rent, false);
        debug_assert_eq!(pre_state_infos.len(), post_state_infos.len());
        if !relax_post_exec_min_balance_check {
            return post_state_infos;
        }
        for (pre, post) in pre_state_infos.iter().zip(&mut post_state_infos) {
            // Grandfather underfunded accounts only when their owner is unchanged,
            // their data does not grow, and their balance does not decrease.
            if matches!(post.rent_state, Some(RentState::RentPaying { .. }))
                && pre.rent_state == Some(RentState::RentExempt)
                && post.balance >= pre.balance
                && post.data_size <= pre.data_size
                && post.owner == pre.owner
            {
                post.rent_state = Some(RentState::RentExempt);
            }
        }
        post_state_infos
    }

    pub(crate) fn verify_changes(
        pre_state_infos: &[Self],
        post_state_infos: &[Self],
        transaction_context: &TransactionContext,
    ) -> Result<()> {
        for (i, (pre_state_info, post_state_info)) in
            pre_state_infos.iter().zip(post_state_infos).enumerate()
        {
            check_rent_state(
                pre_state_info.rent_state.as_ref(),
                post_state_info.rent_state.as_ref(),
                transaction_context,
                i as IndexOfAccount,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_account::AccountSharedData,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::{
            LegacyMessage, Message, MessageHeader, SanitizedMessage,
            compiled_instruction::CompiledInstruction,
        },
        solana_rent::Rent,
        solana_signer::Signer,
        solana_transaction_context::transaction::TransactionContext,
        solana_transaction_error::TransactionError,
        std::collections::HashSet,
    };

    #[test]
    fn test_new() {
        let rent = Rent::default();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey(), key4.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![
                CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![0],
                    data: vec![],
                },
                CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![2],
                    data: vec![],
                },
            ],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message =
            SanitizedMessage::Legacy(LegacyMessage::new(message, &HashSet::new()));

        let transaction_accounts = vec![
            (key1.pubkey(), AccountSharedData::default()),
            (key2.pubkey(), AccountSharedData::default()),
            (key3.pubkey(), AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, rent.clone(), 20, 20, 1);
        let result = TransactionAccountStateInfo::new(&context, &sanitized_message, &rent, false);
        assert_eq!(
            result,
            vec![
                TransactionAccountStateInfo {
                    rent_state: Some(RentState::Uninitialized),
                    ..Default::default()
                },
                TransactionAccountStateInfo::default(),
                TransactionAccountStateInfo {
                    rent_state: Some(RentState::Uninitialized),
                    ..Default::default()
                }
            ]
        );
    }

    #[test]
    #[should_panic(expected = "message and transaction context out of sync, fatal")]
    fn test_new_panic() {
        let rent = Rent::default();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey(), key4.pubkey(), key3.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![
                CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![0],
                    data: vec![],
                },
                CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![2],
                    data: vec![],
                },
            ],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message =
            SanitizedMessage::Legacy(LegacyMessage::new(message, &HashSet::new()));

        let transaction_accounts = vec![
            (key1.pubkey(), AccountSharedData::default()),
            (key2.pubkey(), AccountSharedData::default()),
            (key3.pubkey(), AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, rent.clone(), 20, 20, 1);
        let _result = TransactionAccountStateInfo::new(&context, &sanitized_message, &rent, false);
    }

    #[test]
    fn test_verify_changes() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let pre_rent_state = vec![
            TransactionAccountStateInfo {
                rent_state: Some(RentState::Uninitialized),
                ..Default::default()
            },
            TransactionAccountStateInfo {
                rent_state: Some(RentState::Uninitialized),
                ..Default::default()
            },
        ];
        let post_rent_state = vec![TransactionAccountStateInfo {
            rent_state: Some(RentState::Uninitialized),
            ..Default::default()
        }];

        let transaction_accounts = vec![
            (key1.pubkey(), AccountSharedData::default()),
            (key2.pubkey(), AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, Rent::default(), 20, 20, 1);

        let result = TransactionAccountStateInfo::verify_changes(
            &pre_rent_state,
            &post_rent_state,
            &context,
        );
        assert!(result.is_ok());

        let pre_rent_state = vec![TransactionAccountStateInfo {
            rent_state: Some(RentState::Uninitialized),
            ..Default::default()
        }];
        let post_rent_state = vec![TransactionAccountStateInfo {
            rent_state: Some(RentState::RentPaying { data_size: 2, lamports: 5 }),
            ..Default::default()
        }];

        let transaction_accounts = vec![
            (key1.pubkey(), AccountSharedData::default()),
            (key2.pubkey(), AccountSharedData::default()),
        ];

        let context = TransactionContext::new(transaction_accounts, Rent::default(), 20, 20, 1);
        let result = TransactionAccountStateInfo::verify_changes(
            &pre_rent_state,
            &post_rent_state,
            &context,
        );
        assert_eq!(
            result.err(),
            Some(TransactionError::InsufficientFundsForRent { account_index: 0 })
        );
    }
}
