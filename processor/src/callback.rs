use agave_feature_set::FeatureSet;
use nucleus::Slot;
use solana_account::AccountSharedData;
use solana_precompile_error::PrecompileError;
use solana_pubkey::Pubkey;
use solana_svm::transaction_processing_callback::{
    InvokeContextCallback, TransactionProcessingCallback,
};
use tracing::error;

use accountsdb::AccountLoader;

/// Bridges SVM transaction loading to keeper-backed accounts.
///
/// When `LOAD_OWNED` is set, loaded accounts are copied to owned data; this is
/// used by simulation, which must always operate on owned account copies.
pub(crate) struct LoadCallback<'a, const LOAD_OWNED: bool> {
    /// Reads accounts from the engine's accounts store.
    pub(crate) loader: AccountLoader<'a>,
}

impl<const LO: bool> InvokeContextCallback for LoadCallback<'_, LO> {}

impl<const LOAD_OWNED: bool> TransactionProcessingCallback for LoadCallback<'_, LOAD_OWNED> {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        self.loader
            .load(pubkey)
            .inspect_err(|error| error!(?error, "accountsdb load error"))
            .unwrap_or_default()
            .map(|mut acc| {
                if LOAD_OWNED {
                    acc = acc.owned().into();
                }
                (acc, 0)
            })
    }
}

/// Supplies feature-dependent behavior while the SVM invokes programs.
pub(crate) struct InvokeCallback<'a> {
    /// Active feature set governing precompiles and runtime behavior.
    pub(crate) featureset: &'a FeatureSet,
}

impl InvokeContextCallback for InvokeCallback<'_> {
    fn is_precompile(&self, program_id: &Pubkey) -> bool {
        agave_precompiles::is_precompile(program_id, |id| self.featureset.is_active(id))
    }

    fn process_precompile(
        &self,
        program_id: &Pubkey,
        data: &[u8],
        instruction_datas: Vec<&[u8]>,
    ) -> Result<(), PrecompileError> {
        agave_precompiles::get_precompile(program_id, |id| self.featureset.is_active(id))
            .ok_or(PrecompileError::InvalidPublicKey)
            .and_then(|p| p.verify(data, &instruction_datas, self.featureset))
    }
}
