//! Composing values into sanitized transaction views.

use agave_transaction_view::{
    MAGICBLOCK_INSTRUCTION_TRACE_LENGTH, MAX_MAGICBLOCK_ACCOUNT_LOCKS,
    transaction_version::{MAGICBLOCK_PREFIX, TransactionVersion},
};
use keeper::TransactionView;
use solana_instruction::Instruction;
use solana_message::{
    VersionedMessage,
    v1::{self, SIGNATURE_SIZE},
};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::{Message, Transaction, TransactionError, versioned::VersionedTransaction};

use crate::{Engine, error::EngineError, error::Result};

/// Opaque transaction admitted through the replication verifier trust boundary.
pub struct VerifiedTransaction(pub(crate) TransactionView);

/// Engine-authorized batch verifier for replicated transaction payloads.
#[derive(Clone, Copy)]
pub struct TransactionVerifier {
    authority: Pubkey,
}

impl TransactionVerifier {
    pub(crate) fn new(authority: Pubkey) -> Self {
        Self { authority }
    }

    /// Sanitizes, validates, and batch-verifies every transaction atomically.
    pub fn verify(&self, transactions: Vec<Vec<u8>>) -> Result<Vec<VerifiedTransaction>> {
        let verified = transactions
            .into_iter()
            .map(|transaction| {
                let view = TransactionView::try_new_sanitized(transaction.into(), true)?;
                validate_authority(&view, self.authority)?;
                Ok(VerifiedTransaction(view))
            })
            .collect::<Result<Vec<_>>>()?;

        let mut signatures = Vec::with_capacity(verified.len());
        for transaction in &verified {
            signatures.extend(signature_data(&transaction.0));
        }
        if !Signature::batch_verify(signatures.into_iter()) {
            return Err(EngineError::SignatureVerification);
        }

        Ok(verified)
    }
}

/// Conversion of anything composable into an executable
/// transaction into a sanitized [`TransactionView`].
pub trait IntoTransactionView {
    /// Composes `self` into a sanitized [`TransactionView`], signing with
    /// `engine`'s authority and latest blockhash where applicable.
    fn compose(self, engine: &Engine) -> Result<TransactionView>;
}

impl IntoTransactionView for Message {
    fn compose(self, engine: &Engine) -> Result<TransactionView> {
        let mut transaction = Transaction::new_unsigned(self);
        transaction.try_sign(&[engine.signer()], engine.blockhash())?;
        transaction.compose(engine)
    }
}

impl IntoTransactionView for Transaction {
    fn compose(self, engine: &Engine) -> Result<TransactionView> {
        let data = wincode::serialize(&self).map_err(wincode::Error::from)?;
        data.compose(engine)
    }
}

impl IntoTransactionView for &[Instruction] {
    fn compose(self, engine: &Engine) -> Result<TransactionView> {
        let msg = Message::new(self, Some(&engine.authority()));
        msg.compose(engine)
    }
}

impl<const N: usize> IntoTransactionView for &[Instruction; N] {
    fn compose(self, engine: &Engine) -> Result<TransactionView> {
        self.as_slice().compose(engine)
    }
}

impl IntoTransactionView for Vec<u8> {
    fn compose(self, engine: &Engine) -> Result<TransactionView> {
        TransactionView::try_new_sanitized(self.into(), true)?.compose(engine)
    }
}

impl IntoTransactionView for TransactionView {
    fn compose(self, engine: &Engine) -> Result<TransactionView> {
        validate_authority(&self, engine.authority())?;
        Ok(self)
    }
}

/// Enforces the authority encoded by private Magicblock transactions.
fn validate_authority(view: &TransactionView, authority: Pubkey) -> Result<()> {
    if matches!(view.version(), TransactionVersion::Magicblock)
        && view.static_account_keys()[0] != authority
    {
        return Err(EngineError::SignatureVerification);
    }
    Ok(())
}

/// Iterates each signature with its signer key and shared serialized message.
fn signature_data(
    view: &TransactionView,
) -> impl ExactSizeIterator<Item = (&Signature, &[u8], &[u8])> {
    let message = view.message_data();
    view.signatures()
        .iter()
        .zip(view.static_account_keys())
        .map(move |(signature, key)| (signature, key.as_ref(), message))
}

/// The engine's sole signature-verification point.
///
/// Every public transaction accessor verifies here; trusted local replay is
/// the only bypass. TODO: Remove the bypass before replaying untrusted ledgers.
pub(super) fn sigverify(view: &TransactionView) -> Result<()> {
    for (signature, key, message) in signature_data(view) {
        if !signature.verify(key, message) {
            return Err(EngineError::SignatureVerification);
        }
    }
    Ok(())
}

/// Composes an Engine-private transaction and signs its final
/// Magicblock wire representation with the Engine authority.
pub(crate) fn magicblock(instructions: &[Instruction], engine: &Engine) -> Result<Vec<u8>> {
    let message = v1::Message::try_compile(&engine.authority(), instructions, engine.blockhash())?;
    let message = VersionedMessage::V1(message);
    // These checks are merely future proof defenses, currently it should be
    // impossible to construct a transaction which might violate any of them
    if message.instructions().len() > MAGICBLOCK_INSTRUCTION_TRACE_LENGTH {
        Err(TransactionError::SanitizeFailure)?;
    } else if message.static_account_keys().len() > MAX_MAGICBLOCK_ACCOUNT_LOCKS {
        Err(TransactionError::TooManyAccountLocks)?;
    }
    for ix in message.instructions() {
        if ix.accounts.len() > MAX_MAGICBLOCK_ACCOUNT_LOCKS {
            Err(TransactionError::TooManyAccountLocks)?;
        }
    }

    // Reserve the trailing signature slot without signing the V1 prefix, which
    // is replaced below before the only signing operation.
    let transaction = VersionedTransaction {
        signatures: vec![Default::default()],
        message,
    };
    let mut data = wincode::serialize(&transaction).map_err(wincode::Error::from)?;
    // Patch the transaction prefix to allow for larger tranaction limits
    data[0] = MAGICBLOCK_PREFIX;

    let signature_offset = data.len() - SIGNATURE_SIZE;
    let signature = engine.signer().sign_message(&data[..signature_offset]);
    data[signature_offset..].copy_from_slice(signature.as_ref());
    Ok(data)
}
