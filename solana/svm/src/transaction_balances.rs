#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::field_qualifiers;
use solana_account::ReadableAccount;
use solana_transaction_context::transaction_accounts::KeyedAccountSharedData;

// Use an internal alias so this stays tied to native lamport balances.
type TxNativeBalances = Vec<u64>;

// Implemented for Option<BalanceCollector> to keep call sites branch-free.
pub(crate) trait BalanceCollectionRoutines {
    fn collect_pre_balances(&mut self, accounts: &[KeyedAccountSharedData]);

    fn collect_post_balances(&mut self, accounts: &[KeyedAccountSharedData]);
}

/// Native account balances recorded before and after execution.
#[derive(Debug, Default, Clone)]
#[cfg_attr(
    feature = "dev-context-only-utils",
    field_qualifiers(native_pre(pub), native_post(pub))
)]
pub struct BalanceCollector {
    native_pre: TxNativeBalances,
    native_post: TxNativeBalances,
}

impl BalanceCollector {
    /// Returns recorded pre- and post-execution lamport balances.
    pub fn into_vecs(self) -> (TxNativeBalances, TxNativeBalances) {
        (self.native_pre, self.native_post)
    }

    fn collect_balances(&mut self, accounts: &[KeyedAccountSharedData]) -> TxNativeBalances {
        accounts.iter().map(|a| a.1.lamports()).collect()
    }
}

impl BalanceCollectionRoutines for BalanceCollector {
    fn collect_pre_balances(&mut self, accounts: &[KeyedAccountSharedData]) {
        self.native_pre = self.collect_balances(accounts);
    }

    fn collect_post_balances(&mut self, accounts: &[KeyedAccountSharedData]) {
        self.native_post = self.collect_balances(accounts);
    }
}

impl BalanceCollectionRoutines for Option<BalanceCollector> {
    fn collect_pre_balances(&mut self, accounts: &[KeyedAccountSharedData]) {
        if let Some(inner) = self {
            inner.collect_pre_balances(accounts)
        }
    }

    fn collect_post_balances(&mut self, accounts: &[KeyedAccountSharedData]) {
        if let Some(inner) = self {
            inner.collect_post_balances(accounts)
        }
    }
}
