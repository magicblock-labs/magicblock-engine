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
use solana_signer::Signer;
use solana_transaction::{Message, Transaction, TransactionError, versioned::VersionedTransaction};

use crate::{Engine, error::EngineError, error::Result};

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
        if matches!(self.version(), TransactionVersion::Magicblock)
            && self.static_account_keys()[0] != engine.authority()
        {
            return Err(EngineError::SignatureVerification);
        }
        sigverify(&self)?;
        Ok(self)
    }
}

/// The engine's sole signature-verification point.
///
/// Execution is trustless: every submission funnels through the
/// [`TransactionView`] `compose` and is verified here, including replay and
/// replication of already-committed transactions. No path reaches the
/// sequencer unverified, so downstream code may assume the fee payer and every
/// required signer actually signed.
fn sigverify(view: &TransactionView) -> Result<()> {
    // Sanitization guarantees one static key for every required signature.
    let message = view.message_data();
    for (signature, key) in view.signatures().iter().zip(view.static_account_keys()) {
        if !signature.verify(key.as_ref(), message) {
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
