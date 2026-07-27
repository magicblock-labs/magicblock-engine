use {
    crate::{
        result::{Result, TransactionViewError},
        transaction_data::TransactionData,
        transaction_view::TransactionView,
    },
    core::{
        fmt::{Debug, Formatter},
        ops::Deref,
    },
    solana_hash::Hash,
    solana_message::{AccountKeys, v0::LoadedAddresses},
    solana_pubkey::Pubkey,
    solana_sdk_ids::bpf_loader_upgradeable,
    solana_signature::Signature,
    solana_svm_transaction::{
        instruction::SVMInstruction,
        message_address_table_lookup::SVMMessageAddressTableLookup,
        svm_message::{SVMMessage, SVMStaticMessage},
        svm_transaction::SVMTransaction,
    },
    std::collections::HashSet,
};

/// A parsed and sanitized transaction view with validated loaded-address state.
pub struct ResolvedTransactionView<D: TransactionData> {
    /// The parsed and sanitized transaction view.
    view: TransactionView<true, D>,
    /// The resolved address lookups.
    resolved_addresses: Option<LoadedAddresses>,
    /// A cache for whether an address is writable.
    // Sanitized transactions are guaranteed to have a maximum of 256 keys,
    // because account indexing is done with a u8.
    writable_cache: [bool; 256],
}

impl<D: TransactionData> Deref for ResolvedTransactionView<D> {
    type Target = TransactionView<true, D>;

    fn deref(&self) -> &Self::Target {
        &self.view
    }
}

impl<D: TransactionData> ResolvedTransactionView<D> {
    /// Creates a resolved view after validating any supplied loaded addresses.
    ///
    /// Address lookup tables are rejected during sanitization, so loaded
    /// addresses may only be absent or empty.
    pub fn try_new(
        view: TransactionView<true, D>,
        resolved_addresses: Option<LoadedAddresses>,
        reserved_account_keys: &HashSet<Pubkey>,
    ) -> Result<Self> {
        let resolved_addresses_ref = resolved_addresses.as_ref();

        // Reject unexpected loaded addresses while retaining the upstream API.
        if let Some(loaded_addresses) = resolved_addresses_ref {
            if loaded_addresses.writable.len() != usize::from(view.total_writable_lookup_accounts())
                || loaded_addresses.readonly.len()
                    != usize::from(view.total_readonly_lookup_accounts())
            {
                return Err(TransactionViewError::AddressLookupMismatch);
            }
        } else if view.total_writable_lookup_accounts() != 0
            || view.total_readonly_lookup_accounts() != 0
        {
            return Err(TransactionViewError::AddressLookupMismatch);
        }

        let writable_cache =
            Self::cache_is_writable(&view, resolved_addresses_ref, reserved_account_keys);
        Ok(Self {
            view,
            resolved_addresses,
            writable_cache,
        })
    }

    /// Helper function to check if an address is writable,
    /// and cache the result.
    /// This is done so we avoid recomputing the expensive checks each time we call
    /// `is_writable` - since there is more to it than just checking index.
    fn cache_is_writable(
        view: &TransactionView<true, D>,
        resolved_addresses: Option<&LoadedAddresses>,
        reserved_account_keys: &HashSet<Pubkey>,
    ) -> [bool; 256] {
        // Build account keys so that we can iterate over and check if
        // an address is writable.
        let account_keys = AccountKeys::new(view.static_account_keys(), resolved_addresses);

        let mut is_writable_cache = [false; 256];
        let num_static_account_keys = usize::from(view.num_static_account_keys());
        let num_writable_lookup_accounts = usize::from(view.total_writable_lookup_accounts());
        let num_signed_accounts = usize::from(view.num_required_signatures());
        let num_writable_unsigned_static_accounts =
            usize::from(view.num_writable_unsigned_static_accounts());
        let num_writable_signed_static_accounts =
            usize::from(view.num_writable_signed_static_accounts());

        for (index, key) in account_keys.iter().enumerate() {
            let is_requested_write = {
                // If the account is a resolved address, check if it is writable.
                if index >= num_static_account_keys {
                    let loaded_address_index = index.wrapping_sub(num_static_account_keys);
                    loaded_address_index < num_writable_lookup_accounts
                } else if index >= num_signed_accounts {
                    let unsigned_account_index = index.wrapping_sub(num_signed_accounts);
                    unsigned_account_index < num_writable_unsigned_static_accounts
                } else {
                    index < num_writable_signed_static_accounts
                }
            };

            // If the key is reserved it cannot be writable.
            is_writable_cache[index] = is_requested_write && !reserved_account_keys.contains(key);
        }

        // If a program account is locked, it cannot be writable unless the
        // upgradable loader is present.
        // However, checking for the upgradable loader is somewhat expensive, so
        // we only do it if we find a writable program id.
        let mut is_upgradable_loader_present = None;
        for ix in view.instructions_iter() {
            let program_id_index = usize::from(ix.program_id_index);
            if is_writable_cache[program_id_index]
                && !*is_upgradable_loader_present.get_or_insert_with(|| {
                    for key in account_keys.iter() {
                        if key == &bpf_loader_upgradeable::ID {
                            return true;
                        }
                    }
                    false
                })
            {
                is_writable_cache[program_id_index] = false;
            }
        }

        is_writable_cache
    }

    pub fn loaded_addresses(&self) -> Option<&LoadedAddresses> {
        self.resolved_addresses.as_ref()
    }

    pub fn into_view(self) -> TransactionView<true, D> {
        self.view
    }
}

impl<D: TransactionData> SVMStaticMessage for ResolvedTransactionView<D> {
    fn version(&self) -> solana_transaction::versioned::TransactionVersion {
        self.view.version().into()
    }

    fn num_transaction_signatures(&self) -> u64 {
        u64::from(self.view.num_required_signatures())
    }

    fn num_write_locks(&self) -> u64 {
        self.view.num_requested_write_locks()
    }

    fn recent_blockhash(&self) -> &Hash {
        self.view.recent_blockhash()
    }

    fn num_instructions(&self) -> usize {
        usize::from(self.view.num_instructions())
    }

    fn instructions_iter(&self) -> impl Iterator<Item = SVMInstruction<'_>> {
        self.view.instructions_iter()
    }

    fn program_instructions_iter(
        &self,
    ) -> impl Iterator<
        Item = (
            &solana_pubkey::Pubkey,
            solana_svm_transaction::instruction::SVMInstruction<'_>,
        ),
    > + Clone {
        self.view.program_instructions_iter()
    }

    fn static_account_keys(&self) -> &[Pubkey] {
        self.view.static_account_keys()
    }

    fn fee_payer(&self) -> &Pubkey {
        &self.view.static_account_keys()[0]
    }

    fn num_lookup_tables(&self) -> usize {
        usize::from(self.view.num_address_table_lookups())
    }

    fn message_address_table_lookups(
        &self,
    ) -> impl Iterator<Item = SVMMessageAddressTableLookup<'_>> {
        self.view.address_table_lookup_iter()
    }
}

impl<D: TransactionData> SVMMessage for ResolvedTransactionView<D> {
    fn account_keys(&self) -> AccountKeys<'_> {
        AccountKeys::new(
            self.view.static_account_keys(),
            self.resolved_addresses.as_ref(),
        )
    }

    fn is_writable(&self, index: usize) -> bool {
        self.writable_cache.get(index).copied().unwrap_or(false)
    }

    fn is_signer(&self, index: usize) -> bool {
        index < usize::from(self.view.num_required_signatures())
    }

    fn is_invoked(&self, key_index: usize) -> bool {
        let Ok(index) = u8::try_from(key_index) else {
            return false;
        };
        self.view
            .instructions_iter()
            .any(|ix| ix.program_id_index == index)
    }
}

impl<D: TransactionData> SVMTransaction for ResolvedTransactionView<D> {
    fn signature(&self) -> &Signature {
        &self.view.signatures()[0]
    }

    fn signatures(&self) -> &[Signature] {
        self.view.signatures()
    }
}

impl<D: TransactionData> Debug for ResolvedTransactionView<D> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedTransactionView")
            .field("view", &self.view)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::transaction_view::SanitizedTransactionView,
        solana_message::{
            MessageHeader, VersionedMessage,
            v0::{self, MessageAddressTableLookup},
        },
        solana_signature::Signature,
        solana_transaction::versioned::VersionedTransaction,
    };

    fn v0_transaction(
        address_table_lookups: Vec<MessageAddressTableLookup>,
    ) -> VersionedTransaction {
        VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::V0(v0::Message {
                header: MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                instructions: vec![],
                account_keys: vec![Pubkey::new_unique(), Pubkey::new_unique()],
                address_table_lookups,
                recent_blockhash: Hash::default(),
            }),
        }
    }

    #[test]
    fn test_address_lookup_tables_are_rejected() {
        let lookups = [
            MessageAddressTableLookup {
                account_key: Pubkey::new_unique(),
                writable_indexes: vec![0],
                readonly_indexes: vec![],
            },
            MessageAddressTableLookup {
                account_key: Pubkey::new_unique(),
                writable_indexes: vec![],
                readonly_indexes: vec![0],
            },
            MessageAddressTableLookup {
                account_key: Pubkey::new_unique(),
                writable_indexes: vec![0],
                readonly_indexes: vec![1],
            },
        ];

        for lookup in lookups {
            let transaction = v0_transaction(vec![lookup]);
            let bytes = wincode::serialize(&transaction).unwrap();
            let result = SanitizedTransactionView::try_new_sanitized(bytes.as_ref(), true);
            assert!(matches!(
                result,
                Err(TransactionViewError::AddressLookupMismatch)
            ));
        }
    }

    #[test]
    fn test_v0_without_lookups_needs_no_loaded_addresses() {
        let bytes = wincode::serialize(&v0_transaction(vec![])).unwrap();
        let view = SanitizedTransactionView::try_new_sanitized(bytes.as_ref(), true).unwrap();
        let resolved = ResolvedTransactionView::try_new(view, None, &HashSet::default()).unwrap();
        assert!(resolved.loaded_addresses().is_none());
    }

    #[test]
    fn test_unexpected_loaded_addresses() {
        let loaded_addresses = LoadedAddresses {
            writable: vec![Pubkey::new_unique()],
            readonly: vec![],
        };
        let bytes = wincode::serialize(&v0_transaction(vec![])).unwrap();
        let view = SanitizedTransactionView::try_new_sanitized(bytes.as_ref(), true).unwrap();
        let result =
            ResolvedTransactionView::try_new(view, Some(loaded_addresses), &HashSet::default());
        assert!(matches!(
            result,
            Err(TransactionViewError::AddressLookupMismatch)
        ));
    }
}
