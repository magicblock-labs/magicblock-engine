#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::{field_qualifiers, qualifiers};
use {
    solana_account::{Account, AccountSharedData, PROGRAM_OWNERS, ReadableAccount},
    solana_fee_structure::FeeDetails,
    solana_instruction::{BorrowedAccountMeta, BorrowedInstruction},
    solana_instructions_sysvar::construct_instructions_data,
    solana_program_runtime::execution_budget::{
        SVMTransactionExecutionAndFeeBudgetLimits, SVMTransactionExecutionBudget,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::sysvar,
    solana_svm_callback::TransactionProcessingCallback,
    solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction_context::{IndexOfAccount, transaction_accounts::KeyedAccountSharedData},
    solana_transaction_error::{TransactionError, TransactionResult as Result},
};

// Per SIMD-0186, all accounts are assigned a base size of 64 bytes to cover
// the storage cost of metadata.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) const TRANSACTION_ACCOUNT_BASE_SIZE: usize = 64;

// Per SIMD-0186, resolved address lookup tables are assigned a base size of 8248
// bytes: 8192 bytes for the maximum table size plus 56 bytes for metadata.
const ADDRESS_LOOKUP_TABLE_BASE_SIZE: usize = 8248;

/// Result of transaction prechecking before account loading.
pub type TransactionCheckResult = Result<CheckedTransactionDetails>;

#[derive(PartialEq, Eq, Debug)]
pub(crate) enum TransactionLoadResult {
    /// All transaction accounts and executable program accounts were resolved.
    Loaded(LoadedTransaction),
    /// Loading failed before execution could start.
    NotLoaded(TransactionError),
}

/// Transaction limits and metadata computed before account loading.
#[derive(PartialEq, Eq, Debug, Clone)]
#[cfg_attr(feature = "svm-internal", qualifier_attr::field_qualifiers(nonce_address(pub)))]
pub struct CheckedTransactionDetails {
    pub(crate) nonce_address: Option<Pubkey>,
    pub(crate) compute_budget_and_limits: SVMTransactionExecutionAndFeeBudgetLimits,
}

impl Default for CheckedTransactionDetails {
    fn default() -> Self {
        Self {
            nonce_address: None,
            compute_budget_and_limits: SVMTransactionExecutionAndFeeBudgetLimits {
                budget: SVMTransactionExecutionBudget::default(),
                loaded_accounts_data_size_limit: 32,
                fee_details: FeeDetails::default(),
            },
        }
    }
}

impl CheckedTransactionDetails {
    /// Creates checked transaction details from caller-provided validation.
    pub fn new(
        nonce_address: Option<Pubkey>,
        compute_budget_and_limits: SVMTransactionExecutionAndFeeBudgetLimits,
    ) -> Self {
        Self {
            nonce_address,
            compute_budget_and_limits,
        }
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub(crate) struct ValidatedTransactionDetails {
    pub(crate) compute_budget: SVMTransactionExecutionBudget,
    pub(crate) loaded_accounts_bytes_limit: u32,
    pub(crate) fee_details: FeeDetails,
}

#[cfg(feature = "dev-context-only-utils")]
impl Default for ValidatedTransactionDetails {
    fn default() -> Self {
        Self {
            compute_budget: SVMTransactionExecutionBudget::default(),
            loaded_accounts_bytes_limit:
                solana_program_runtime::execution_budget::MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get(),
            fee_details: FeeDetails::default(),
        }
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
#[cfg_attr(feature = "dev-context-only-utils", derive(Default))]
pub(crate) struct LoadedTransactionAccount {
    pub(crate) account: AccountSharedData,
    pub(crate) loaded_size: usize,
}

impl LoadedTransactionAccount {
    fn new(account: AccountSharedData) -> Self {
        Self {
            loaded_size: TRANSACTION_ACCOUNT_BASE_SIZE.saturating_add(account.data().len()),
            account,
        }
    }
}

/// Accounts and execution metadata needed to run one transaction.
#[derive(PartialEq, Eq, Debug, Clone, Default)]
#[cfg_attr(
    feature = "dev-context-only-utils",
    field_qualifiers(program_indices(pub), compute_budget(pub))
)]
pub struct LoadedTransaction {
    /// Transaction accounts in message account-key order.
    pub accounts: Vec<KeyedAccountSharedData>,
    pub(crate) program_indices: Vec<IndexOfAccount>,
    /// Fee metadata carried through for callers that still consume it.
    pub fee_details: FeeDetails,
    pub(crate) compute_budget: SVMTransactionExecutionBudget,
    /// Total loaded account data size charged against the transaction limit.
    pub loaded_accounts_data_size: u32,
}

pub(crate) fn load_transaction<CB: TransactionProcessingCallback>(
    account_loader: &CB,
    message: &impl SVMMessage,
    validation_details: ValidatedTransactionDetails,
) -> TransactionLoadResult {
    let load_result = load_transaction_accounts(
        account_loader,
        message,
        validation_details.loaded_accounts_bytes_limit,
    );

    match load_result {
        Ok(loaded_tx_accounts) => TransactionLoadResult::Loaded(LoadedTransaction {
            accounts: loaded_tx_accounts.accounts,
            program_indices: loaded_tx_accounts.program_indices,
            fee_details: validation_details.fee_details,
            compute_budget: validation_details.compute_budget,
            loaded_accounts_data_size: loaded_tx_accounts.loaded_accounts_data_size,
        }),
        Err(err) => TransactionLoadResult::NotLoaded(err),
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
struct LoadedTransactionAccounts {
    pub(crate) accounts: Vec<KeyedAccountSharedData>,
    pub(crate) program_indices: Vec<IndexOfAccount>,
    pub(crate) loaded_accounts_data_size: u32,
}

impl LoadedTransactionAccounts {
    fn increase_calculated_data_size(
        &mut self,
        data_size_delta: usize,
        requested_loaded_accounts_data_size_limit: u32,
    ) -> Result<()> {
        let Ok(data_size_delta) = u32::try_from(data_size_delta) else {
            return Err(TransactionError::MaxLoadedAccountsDataSizeExceeded);
        };

        self.loaded_accounts_data_size =
            self.loaded_accounts_data_size.saturating_add(data_size_delta);

        if self.loaded_accounts_data_size > requested_loaded_accounts_data_size_limit {
            Err(TransactionError::MaxLoadedAccountsDataSizeExceeded)
        } else {
            Ok(())
        }
    }
}

fn load_transaction_accounts<CB: TransactionProcessingCallback>(
    account_loader: &CB,
    message: &impl SVMMessage,
    loaded_accounts_bytes_limit: u32,
) -> Result<LoadedTransactionAccounts> {
    let account_keys = message.account_keys();

    let mut loaded_transaction_accounts = LoadedTransactionAccounts {
        accounts: Vec::with_capacity(account_keys.len()),
        program_indices: Vec::with_capacity(message.num_instructions()),
        loaded_accounts_data_size: 0,
    };

    // Transactions pay a base fee per address lookup table.
    loaded_transaction_accounts.increase_calculated_data_size(
        message.num_lookup_tables().saturating_mul(ADDRESS_LOOKUP_TABLE_BASE_SIZE),
        loaded_accounts_bytes_limit,
    )?;

    let mut collect_loaded_account = |key: &Pubkey, loaded_account| -> Result<()> {
        let LoadedTransactionAccount { account, loaded_size } = loaded_account;

        loaded_transaction_accounts
            .increase_calculated_data_size(loaded_size, loaded_accounts_bytes_limit)?;

        loaded_transaction_accounts.accounts.push((*key, account));

        Ok(())
    };

    // Attempt to load all of the transaction accounts
    for account_key in account_keys.iter() {
        let loaded_account = load_transaction_account(account_loader, message, account_key);
        collect_loaded_account(account_key, loaded_account)?;
    }

    for (program_id, instruction) in message.program_instructions_iter() {
        let Some(program_account) = account_loader.get_account_shared_data(program_id) else {
            return Err(TransactionError::ProgramAccountNotFound);
        };

        let owner_id = program_account.0.owner();
        if !PROGRAM_OWNERS.contains(owner_id) {
            return Err(TransactionError::InvalidProgramForExecution);
        }

        loaded_transaction_accounts
            .program_indices
            .push(instruction.program_id_index as IndexOfAccount);
    }

    Ok(loaded_transaction_accounts)
}

fn load_transaction_account<CB: TransactionProcessingCallback>(
    account_loader: &CB,
    message: &impl SVMMessage,
    account_key: &Pubkey,
) -> LoadedTransactionAccount {
    if solana_sdk_ids::sysvar::instructions::check_id(account_key) {
        // Since the instructions sysvar is constructed by the SVM and modified
        // for each transaction instruction, it cannot be loaded.
        return LoadedTransactionAccount {
            loaded_size: 0,
            account: construct_instructions_account(message),
        };
    }
    account_loader
        .get_account_shared_data(account_key)
        .map(|a| LoadedTransactionAccount::new(a.0))
        .unwrap_or_else(|| LoadedTransactionAccount::new(Default::default()))
}

fn construct_instructions_account(message: &impl SVMMessage) -> AccountSharedData {
    let account_keys = message.account_keys();
    let mut decompiled_instructions = Vec::with_capacity(message.num_instructions());
    for (program_id, instruction) in message.program_instructions_iter() {
        let accounts = instruction
            .accounts
            .iter()
            .map(|account_index| {
                let account_index = usize::from(*account_index);
                BorrowedAccountMeta {
                    is_signer: message.is_signer(account_index),
                    is_writable: message.is_writable(account_index),
                    pubkey: account_keys.get(account_index).unwrap(),
                }
            })
            .collect();

        decompiled_instructions.push(BorrowedInstruction {
            accounts,
            data: instruction.data,
            program_id,
        });
    }

    AccountSharedData::from(Account {
        data: construct_instructions_data(&decompiled_instructions).unwrap_or_default(),
        owner: sysvar::id(),
        ..Account::default()
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            rent_calculator::RENT_EXEMPT_RENT_EPOCH,
            transaction_account_state_info::TransactionAccountStateInfo,
        },
        ahash::AHashMap,
        rand::prelude::*,
        solana_account::{
            Account, AccountSharedData, ReadableAccount, WritableAccount, state_traits::StateMut,
        },
        solana_clock::Slot,
        solana_hash::Hash,
        solana_instruction::{AccountMeta, Instruction},
        solana_keypair::Keypair,
        solana_loader_v3_interface::state::UpgradeableLoaderState,
        solana_message::{
            LegacyMessage, Message, MessageHeader, SanitizedMessage,
            compiled_instruction::CompiledInstruction,
            v0::{LoadedAddresses, LoadedMessage},
        },
        solana_native_token::LAMPORTS_PER_SOL,
        solana_program_runtime::execution_budget::{
            DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT, MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
        },
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_sdk_ids::{
            bpf_loader, bpf_loader_upgradeable, native_loader, system_program, sysvar,
        },
        solana_signature::Signature,
        solana_signer::Signer,
        solana_svm_callback::{AccountState, InvokeContextCallback, TransactionProcessingCallback},
        solana_system_interface::instruction as system_instruction,
        solana_transaction::{Transaction, sanitized::SanitizedTransaction},
        solana_transaction_context::{
            transaction::TransactionContext, transaction_accounts::KeyedAccountSharedData,
        },
        solana_transaction_error::TransactionError,
        std::{
            borrow::Cow,
            cell::RefCell,
            collections::{HashMap, HashSet},
            sync::Arc,
        },
    };

    fn setup_test_logger() {
        let _ = env_logger::Builder::from_env(env_logger::Env::new().default_filter_or("error"))
            .format_timestamp_nanos()
            .is_test(true)
            .try_init();
    }

    #[derive(Clone, Default)]
    struct TestCallbacks {
        accounts_map: HashMap<Pubkey, (AccountSharedData, Slot)>,
        #[allow(clippy::type_complexity)]
        inspected_accounts:
            RefCell<HashMap<Pubkey, Vec<(Option<AccountSharedData>, /* is_writable */ bool)>>>,
    }

    impl InvokeContextCallback for TestCallbacks {}

    impl TransactionProcessingCallback for TestCallbacks {
        fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
            self.accounts_map.get(pubkey).map(|(account, slot)| (account.clone(), *slot))
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
                .borrow_mut()
                .entry(*address)
                .or_default()
                .push((account, is_writable));
        }
    }

    fn load_accounts_with_features_and_rent(
        tx: Transaction,
        accounts: &[KeyedAccountSharedData],
    ) -> TransactionLoadResult {
        let sanitized_tx = SanitizedTransaction::from_transaction_for_tests(tx);
        let mut accounts_map = HashMap::new();
        for (pubkey, account) in accounts {
            accounts_map.insert(*pubkey, (account.clone(), 1));
        }
        let callbacks = TestCallbacks {
            accounts_map,
            ..Default::default()
        };
        load_transaction(
            &callbacks,
            &sanitized_tx,
            ValidatedTransactionDetails::default(),
        )
    }

    fn new_unchecked_sanitized_message(message: Message) -> SanitizedMessage {
        SanitizedMessage::Legacy(LegacyMessage::new(message, &HashSet::new()))
    }

    #[test]
    fn test_load_accounts_unknown_program_id() {
        let mut accounts: Vec<KeyedAccountSharedData> = Vec::new();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);

        let account = AccountSharedData::new(1, 0, &Pubkey::default());
        accounts.push((key0, account));

        let account = AccountSharedData::new(2, 1, &Pubkey::default());
        accounts.push((key1, account));

        let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![Pubkey::default()],
            instructions,
        );

        let load_results = load_accounts_with_features_and_rent(tx, &accounts);

        assert!(matches!(
            load_results,
            TransactionLoadResult::NotLoaded(TransactionError::ProgramAccountNotFound),
        ));
    }

    #[test]
    fn test_load_accounts_no_loaders() {
        let mut accounts: Vec<KeyedAccountSharedData> = Vec::new();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);

        let mut account = AccountSharedData::new(1, 0, &Pubkey::default());
        account.set_rent_epoch(1);
        accounts.push((key0, account));

        let mut account = AccountSharedData::new(2, 1, &Pubkey::default());
        account.set_rent_epoch(1);
        accounts.push((key1, account));

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[key1],
            Hash::default(),
            vec![native_loader::id()],
            instructions,
        );

        let loaded_accounts = load_accounts_with_features_and_rent(tx, &accounts);

        match &loaded_accounts {
            TransactionLoadResult::NotLoaded(err) => {
                assert_eq!(*err, TransactionError::ProgramAccountNotFound);
            }
            result => panic!("unexpected result: {result:?}"),
        }
    }

    #[test]
    fn test_load_accounts_bad_owner() {
        let mut accounts: Vec<KeyedAccountSharedData> = Vec::new();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);

        let account = AccountSharedData::new(1, 0, &Pubkey::default());
        accounts.push((key0, account));

        let mut account = AccountSharedData::new(40, 1, &Pubkey::default());
        account.set_executable(true);
        accounts.push((key1, account));

        let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![key1],
            instructions,
        );

        let load_results = load_accounts_with_features_and_rent(tx, &accounts);

        assert!(matches!(
            load_results,
            TransactionLoadResult::NotLoaded(TransactionError::InvalidProgramForExecution),
        ));
    }

    #[test]
    fn test_load_accounts_not_executable() {
        let mut accounts: Vec<KeyedAccountSharedData> = Vec::new();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);

        let account = AccountSharedData::new(1, 0, &Pubkey::default());
        accounts.push((key0, account));

        let account = AccountSharedData::new(40, 0, &native_loader::id());
        accounts.push((key1, account));

        let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![key1],
            instructions,
        );

        let load_results = load_accounts_with_features_and_rent(tx, &accounts);

        match &load_results {
            TransactionLoadResult::Loaded(loaded_transaction) => {
                assert_eq!(loaded_transaction.accounts.len(), 2);
                assert_eq!(loaded_transaction.accounts[0].1, accounts[0].1);
                assert_eq!(loaded_transaction.accounts[1].1, accounts[1].1);
                assert_eq!(loaded_transaction.program_indices.len(), 1);
                assert_eq!(loaded_transaction.program_indices[0], 1);
            }
            TransactionLoadResult::NotLoaded(e) => panic!("{e}"),
        }
    }

    #[test]
    fn test_load_accounts_multiple_loaders() {
        let mut accounts: Vec<KeyedAccountSharedData> = Vec::new();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = bpf_loader_upgradeable::id();
        let key2 = Pubkey::from([6u8; 32]);

        let mut account = AccountSharedData::new(1, 0, &Pubkey::default());
        account.set_rent_epoch(1);
        accounts.push((key0, account));

        let mut account = AccountSharedData::new(40, 1, &Pubkey::default());
        account.set_executable(true);
        account.set_rent_epoch(1);
        account.set_owner(native_loader::id());
        accounts.push((key1, account));

        let mut account = AccountSharedData::new(41, 1, &Pubkey::default());
        account.set_executable(true);
        account.set_rent_epoch(1);
        account.set_owner(key1);
        accounts.push((key2, account));

        let instructions = vec![
            CompiledInstruction::new(1, &(), vec![0]),
            CompiledInstruction::new(2, &(), vec![0]),
        ];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![key1, key2],
            instructions,
        );

        let loaded_accounts = load_accounts_with_features_and_rent(tx, &accounts);

        match &loaded_accounts {
            TransactionLoadResult::Loaded(loaded_transaction) => {
                assert_eq!(loaded_transaction.accounts.len(), 3);
                assert_eq!(loaded_transaction.accounts[0].1, accounts[0].1);
                assert_eq!(loaded_transaction.program_indices.len(), 2);
                assert_eq!(loaded_transaction.program_indices[0], 1);
                assert_eq!(loaded_transaction.program_indices[1], 2);
            }
            TransactionLoadResult::NotLoaded(e) => panic!("{e}"),
        }
    }

    fn load_accounts_no_store(
        accounts: &[KeyedAccountSharedData],
        tx: Transaction,
    ) -> TransactionLoadResult {
        let tx = SanitizedTransaction::from_transaction_for_tests(tx);

        let mut accounts_map = HashMap::new();
        for (pubkey, account) in accounts {
            accounts_map.insert(*pubkey, (account.clone(), 1));
        }
        let callbacks = TestCallbacks {
            accounts_map,
            ..Default::default()
        };
        load_transaction(&callbacks, &tx, ValidatedTransactionDetails::default())
    }

    #[test]
    fn test_instructions() {
        setup_test_logger();
        let instructions_key = solana_sdk_ids::sysvar::instructions::id();
        let keypair = Keypair::new();
        let instructions = vec![CompiledInstruction::new(1, &(), vec![0, 1])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[solana_pubkey::new_rand(), instructions_key],
            Hash::default(),
            vec![native_loader::id()],
            instructions,
        );

        let load_results = load_accounts_no_store(&[], tx);
        assert!(matches!(
            load_results,
            TransactionLoadResult::NotLoaded(TransactionError::ProgramAccountNotFound),
        ));
    }

    #[test]
    fn test_increase_calculated_data_size() {
        let mut acc = LoadedTransactionAccounts {
            accounts: vec![],
            program_indices: vec![],
            loaded_accounts_data_size: 0,
        };

        let data_size: usize = 123;
        let requested_data_size_limit = data_size as u32;

        // OK - loaded data size is up to limit
        assert!(acc.increase_calculated_data_size(data_size, requested_data_size_limit).is_ok());
        assert_eq!(data_size as u32, acc.loaded_accounts_data_size);

        // fail - loading more data that would exceed limit
        let another_byte: usize = 1;
        assert_eq!(
            acc.increase_calculated_data_size(another_byte, requested_data_size_limit),
            Err(TransactionError::MaxLoadedAccountsDataSizeExceeded)
        );
    }

    #[test]
    fn test_construct_instructions_account() {
        let loaded_message = LoadedMessage {
            message: Cow::Owned(solana_message::v0::Message::default()),
            loaded_addresses: Cow::Owned(LoadedAddresses::default()),
            is_writable_account_cache: vec![false],
        };
        let message = SanitizedMessage::V0(loaded_message);
        let shared_data = construct_instructions_account(&message);
        let expected = AccountSharedData::from(Account {
            data: construct_instructions_data(&message.decompile_instructions()).unwrap(),
            owner: sysvar::id(),
            ..Account::default()
        });
        assert_eq!(shared_data, expected);
    }

    #[test]
    fn test_load_transaction_accounts_fee_payer() {
        let fee_payer_address = Pubkey::new_unique();
        let message = Message {
            account_keys: vec![fee_payer_address],
            header: MessageHeader::default(),
            instructions: vec![],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();

        let fee_payer_balance = 200;
        let mut fee_payer_account = AccountSharedData::default();
        fee_payer_account.set_lamports(fee_payer_balance);
        mock_bank.accounts_map.insert(fee_payer_address, (fee_payer_account.clone(), 1));
        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get(),
        );
        assert_eq!(
            result.unwrap(),
            LoadedTransactionAccounts {
                accounts: vec![(fee_payer_address, fee_payer_account)],
                program_indices: vec![],
                loaded_accounts_data_size: TRANSACTION_ACCOUNT_BASE_SIZE as u32,
            }
        );
    }

    #[test]
    fn test_load_transaction_accounts_native_loader() {
        let key1 = Keypair::new();
        let message = Message {
            account_keys: vec![key1.pubkey(), native_loader::id()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        mock_bank
            .accounts_map
            .insert(native_loader::id(), (AccountSharedData::default(), 0));
        let mut fee_payer_account = AccountSharedData::default();
        fee_payer_account.set_lamports(200);
        mock_bank.accounts_map.insert(key1.pubkey(), (fee_payer_account.clone(), 1));
        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get(),
        );

        assert_eq!(
            result.unwrap_err(),
            TransactionError::InvalidProgramForExecution
        );
    }

    #[test]
    fn test_load_transaction_accounts_program_account_no_data() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();

        let message = Message {
            account_keys: vec![key1.pubkey(), key2.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0, 1],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(200);
        mock_bank.accounts_map.insert(key1.pubkey(), (account_data, 1));
        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get(),
        );

        assert_eq!(result.err(), Some(TransactionError::ProgramAccountNotFound));
    }

    #[test]
    fn test_load_transaction_accounts_invalid_program_for_execution() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();

        let message = Message {
            account_keys: vec![key1.pubkey(), key2.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 0,
                accounts: vec![0, 1],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(200);
        mock_bank.accounts_map.insert(key1.pubkey(), (account_data, 1));
        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get(),
        );

        assert_eq!(
            result.err(),
            Some(TransactionError::InvalidProgramForExecution)
        );
    }

    #[test]
    fn test_load_transaction_accounts_native_loader_owner() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(native_loader::id());
        account_data.set_lamports(1);
        account_data.set_executable(true);
        mock_bank.accounts_map.insert(key1.pubkey(), (account_data, 1));

        let mut fee_payer_account = AccountSharedData::default();
        fee_payer_account.set_lamports(200);
        mock_bank.accounts_map.insert(key2.pubkey(), (fee_payer_account.clone(), 1));
        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get(),
        );

        let loaded_accounts_data_size = TRANSACTION_ACCOUNT_BASE_SIZE as u32 * 2;

        assert_eq!(
            result.unwrap(),
            LoadedTransactionAccounts {
                accounts: vec![
                    (key2.pubkey(), fee_payer_account),
                    (
                        key1.pubkey(),
                        mock_bank.accounts_map[&key1.pubkey()].0.clone()
                    ),
                ],
                program_indices: vec![1],
                loaded_accounts_data_size,
            }
        );
    }

    #[test]
    fn test_load_transaction_accounts_program_account_not_found_after_all_checks() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_executable(true);
        mock_bank.accounts_map.insert(key1.pubkey(), (account_data, 1));

        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(200);
        mock_bank.accounts_map.insert(key2.pubkey(), (account_data, 1));
        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get(),
        );

        assert_eq!(
            result.err(),
            Some(TransactionError::InvalidProgramForExecution)
        );
    }

    #[test]
    fn test_load_transaction_accounts_program_account_invalid_program_for_execution_last_check() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(key3.pubkey());
        mock_bank.accounts_map.insert(key1.pubkey(), (account_data, 1));

        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(200);
        mock_bank.accounts_map.insert(key2.pubkey(), (account_data, 1));
        mock_bank.accounts_map.insert(key3.pubkey(), (AccountSharedData::default(), 0));
        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get(),
        );

        assert_eq!(
            result.err(),
            Some(TransactionError::InvalidProgramForExecution)
        );
    }

    #[test]
    fn test_load_transaction_accounts_program_success_complete() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(bpf_loader::id());
        mock_bank.accounts_map.insert(key1.pubkey(), (account_data, 1));

        let mut fee_payer_account = AccountSharedData::default();
        fee_payer_account.set_lamports(200);
        mock_bank.accounts_map.insert(key2.pubkey(), (fee_payer_account.clone(), 1));

        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(native_loader::id());
        mock_bank.accounts_map.insert(bpf_loader::id(), (account_data, 0));
        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get(),
        );

        let loaded_accounts_data_size = TRANSACTION_ACCOUNT_BASE_SIZE as u32 * 2;

        assert_eq!(
            result.unwrap(),
            LoadedTransactionAccounts {
                accounts: vec![
                    (key2.pubkey(), fee_payer_account),
                    (
                        key1.pubkey(),
                        mock_bank.accounts_map[&key1.pubkey()].0.clone()
                    ),
                ],
                program_indices: vec![1],
                loaded_accounts_data_size,
            }
        );
    }

    #[test]
    fn test_load_transaction_accounts_program_builtin_saturating_add() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey(), key3.pubkey()],
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

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(bpf_loader::id());
        mock_bank.accounts_map.insert(key1.pubkey(), (account_data, 0));

        let mut fee_payer_account = AccountSharedData::default();
        fee_payer_account.set_lamports(200);
        mock_bank.accounts_map.insert(key2.pubkey(), (fee_payer_account.clone(), 1));

        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(native_loader::id());
        mock_bank.accounts_map.insert(bpf_loader::id(), (account_data, 0));
        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get(),
        );

        let loaded_accounts_data_size = TRANSACTION_ACCOUNT_BASE_SIZE as u32 * 3;

        let mut account_data = AccountSharedData::default();
        account_data.set_rent_epoch(RENT_EXEMPT_RENT_EPOCH);
        assert_eq!(
            result.unwrap(),
            LoadedTransactionAccounts {
                accounts: vec![
                    (key2.pubkey(), fee_payer_account),
                    (
                        key1.pubkey(),
                        mock_bank.accounts_map[&key1.pubkey()].0.clone()
                    ),
                    (key3.pubkey(), account_data),
                ],
                program_indices: vec![1, 1],
                loaded_accounts_data_size,
            }
        );
    }

    #[test]
    fn test_rent_state_list_len() {
        let mint_keypair = Keypair::new();
        let mut bank = TestCallbacks::default();
        let recipient = Pubkey::new_unique();
        let last_block_hash = Hash::new_unique();

        let mut system_data = AccountSharedData::default();
        system_data.set_lamports(1);
        system_data.set_executable(true);
        system_data.set_owner(native_loader::id());
        bank.accounts_map.insert(Pubkey::new_from_array([0u8; 32]), (system_data, 0));

        let mut mint_data = AccountSharedData::default();
        mint_data.set_lamports(2);
        bank.accounts_map.insert(mint_keypair.pubkey(), (mint_data, 0));
        bank.accounts_map.insert(recipient, (AccountSharedData::default(), 1));
        let mut tx = Transaction::new_with_payer(
            &[system_instruction::transfer(
                &mint_keypair.pubkey(),
                &recipient,
                LAMPORTS_PER_SOL,
            )],
            Some(&mint_keypair.pubkey()),
        );
        tx.sign(&[&mint_keypair], last_block_hash);
        let num_accounts = tx.message().account_keys.len();
        let sanitized_tx = SanitizedTransaction::from_transaction_for_tests(tx);
        let load_result =
            load_transaction(&bank, &sanitized_tx, ValidatedTransactionDetails::default());

        let TransactionLoadResult::Loaded(loaded_transaction) = load_result else {
            panic!("transaction loading failed");
        };

        let compute_budget = SVMTransactionExecutionBudget {
            compute_unit_limit: u64::from(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT),
            ..SVMTransactionExecutionBudget::default()
        };
        let rent = Rent::default();
        let transaction_context = TransactionContext::new(
            loaded_transaction.accounts,
            rent.clone(),
            compute_budget.max_instruction_stack_depth,
            compute_budget.max_instruction_trace_length,
            1,
        );

        assert_eq!(
            TransactionAccountStateInfo::new(&transaction_context, sanitized_tx.message(), &rent,)
                .len(),
            num_accounts,
        );
    }

    #[test]
    fn test_load_accounts_success() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey(), key3.pubkey()],
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

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(bpf_loader::id());
        mock_bank.accounts_map.insert(key1.pubkey(), (account_data, 0));

        let mut fee_payer_account = AccountSharedData::default();
        fee_payer_account.set_lamports(200);
        mock_bank.accounts_map.insert(key2.pubkey(), (fee_payer_account.clone(), 1));

        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(native_loader::id());
        mock_bank.accounts_map.insert(bpf_loader::id(), (account_data, 0));
        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let load_result = load_transaction(
            &mock_bank,
            &sanitized_transaction,
            ValidatedTransactionDetails::default(),
        );

        let loaded_accounts_data_size = TRANSACTION_ACCOUNT_BASE_SIZE as u32 * 3;

        let mut account_data = AccountSharedData::default();
        account_data.set_rent_epoch(RENT_EXEMPT_RENT_EPOCH);

        let TransactionLoadResult::Loaded(loaded_transaction) = load_result else {
            panic!("transaction loading failed");
        };
        assert_eq!(
            loaded_transaction,
            LoadedTransaction {
                accounts: vec![
                    (
                        key2.pubkey(),
                        mock_bank.accounts_map[&key2.pubkey()].0.clone()
                    ),
                    (
                        key1.pubkey(),
                        mock_bank.accounts_map[&key1.pubkey()].0.clone()
                    ),
                    (key3.pubkey(), account_data),
                ],
                program_indices: vec![1, 1],
                fee_details: FeeDetails::default(),
                compute_budget: SVMTransactionExecutionBudget::default(),
                loaded_accounts_data_size,
            }
        );
    }

    #[test]
    fn test_load_accounts_error() {
        let mock_bank = TestCallbacks::default();
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
        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let load_result = load_transaction(
            &mock_bank,
            &sanitized_transaction,
            ValidatedTransactionDetails::default(),
        );

        assert!(matches!(
            load_result,
            TransactionLoadResult::NotLoaded(TransactionError::ProgramAccountNotFound),
        ));
    }

    // note all magic numbers (how many accounts, how many instructions, how big to size buffers) are arbitrary
    // other than trying not to swamp programs with blank accounts and keep transaction size below the 64mb limit
    #[test]
    fn test_load_transaction_accounts_data_sizes() {
        let mut rng = rand::rng();
        let mut mock_bank = TestCallbacks::default();

        // arbitrary accounts
        for _ in 0..128 {
            let account = AccountSharedData::create_from_existing_shared_data(
                1,
                Arc::new(vec![0; rng.random_range(0..128)]),
                Pubkey::new_unique(),
                rng.random(),
                u64::MAX,
            );
            mock_bank.accounts_map.insert(Pubkey::new_unique(), (account, 1));
        }

        // fee-payers
        let mut fee_payers = vec![];
        for _ in 0..8 {
            let fee_payer = Pubkey::new_unique();
            let account = AccountSharedData::create_from_existing_shared_data(
                LAMPORTS_PER_SOL,
                Arc::new(vec![0; rng.random_range(0..32)]),
                system_program::id(),
                rng.random(),
                u64::MAX,
            );
            mock_bank.accounts_map.insert(fee_payer, (account, 1));
            fee_payers.push(fee_payer);
        }

        // programs
        let mut loader_owned_accounts = vec![];
        let mut programdata_tracker = AHashMap::new();
        for loader in PROGRAM_OWNERS {
            for _ in 0..16 {
                let program_id = Pubkey::new_unique();
                let mut account = AccountSharedData::create_from_existing_shared_data(
                    1,
                    Arc::new(vec![0; rng.random_range(0..512)]),
                    *loader,
                    rng.random(),
                    u64::MAX,
                );

                // give half loaderv3 accounts (if they're long enough) a valid programdata
                // a quarter a dead pointer and a quarter nothing
                // we set executable like a program because after the flag is disabled...
                // ...programdata and buffer accounts can be used as program ids without aborting loading
                // this will always fail at execution but we are merely testing the data size accounting here
                if *loader == bpf_loader_upgradeable::id() && account.data().len() >= 64 {
                    let programdata_address = Pubkey::new_unique();
                    let has_programdata = rng.random();

                    if has_programdata {
                        let programdata_account =
                            AccountSharedData::create_from_existing_shared_data(
                                1,
                                Arc::new(vec![0; rng.random_range(0..512)]),
                                *loader,
                                rng.random(),
                                u64::MAX,
                            );
                        programdata_tracker.insert(
                            program_id,
                            (programdata_address, programdata_account.data().len()),
                        );
                        mock_bank
                            .accounts_map
                            .insert(programdata_address, (programdata_account, 1));
                        loader_owned_accounts.push(programdata_address);
                    }

                    if has_programdata || rng.random() {
                        account
                            .set_state(&UpgradeableLoaderState::Program { programdata_address })
                            .unwrap();
                    }
                }

                mock_bank.accounts_map.insert(program_id, (account, 1));
                loader_owned_accounts.push(program_id);
            }
        }

        let mut all_accounts = mock_bank.accounts_map.keys().copied().collect::<Vec<_>>();

        // Append some missing accounts. The current loader materializes them as
        // default accounts, so they still contribute the base account size.
        for _ in 0..32 {
            all_accounts.push(Pubkey::new_unique());
        }

        // now generate arbitrary transactions using this accounts
        // we ensure valid fee-payers and that all program ids are loader-owned
        // otherwise any account can appear anywhere
        // some edge cases we hope to hit (not necessarily all in every run):
        // * programs used multiple times as program ids and/or normal accounts are counted once
        // * loaderv3 programdata used explicitly zero one or multiple times is counted once
        // * loaderv3 programs with missing programdata are allowed through
        // * loaderv3 programdata used as program id does nothing weird
        // * loaderv3 programdata used as a regular account does nothing weird
        // * the programdata conditions hold regardless of ordering
        for _ in 0..1024 {
            let mut instructions = vec![];
            for _ in 0..rng.random_range(1..8) {
                let mut accounts = vec![];
                for _ in 0..rng.random_range(1..16) {
                    all_accounts.shuffle(&mut rng);
                    let pubkey = all_accounts[0];

                    accounts.push(AccountMeta {
                        pubkey,
                        is_writable: rng.random(),
                        is_signer: rng.random() && rng.random(),
                    });
                }

                loader_owned_accounts.shuffle(&mut rng);
                let program_id = loader_owned_accounts[0];
                instructions.push(Instruction {
                    accounts,
                    program_id,
                    data: vec![],
                });
            }

            fee_payers.shuffle(&mut rng);
            let fee_payer = fee_payers[0];
            let transaction = SanitizedTransaction::from_transaction_for_tests(
                Transaction::new_with_payer(&instructions, Some(&fee_payer)),
            );

            let mut expected_size = 0;
            for pubkey in transaction.account_keys().iter() {
                let account_data_len = mock_bank
                    .accounts_map
                    .get(pubkey)
                    .map(|(account, _last_modification_slot)| account.data().len())
                    .unwrap_or_default();
                expected_size += TRANSACTION_ACCOUNT_BASE_SIZE + account_data_len;
            }

            assert!(expected_size <= MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get() as usize);

            let loaded_transaction_accounts = load_transaction_accounts(
                &mock_bank,
                &transaction,
                MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get(),
            )
            .unwrap();

            assert_eq!(
                loaded_transaction_accounts.loaded_accounts_data_size,
                expected_size as u32,
            );
        }
    }
}
