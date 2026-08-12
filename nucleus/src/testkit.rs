//! Engine-agnostic test fixtures shared across crate test suites.
//!
//! Only the primitives that depend on nothing above nucleus live here —
//! wincode-serialized transactions, block boundaries, and throwaway directories.
//! Keeper-level harness code (building a `Keeper`, loading the v42 ELF) lives in
//! `keeper::testkit`. Compiled only under the `testkit` feature, so it never
//! reaches release builds.
// Test-support code: a panic here fails the test that caused it, which is the
// intended reporting path. Kept out of release builds by the `testkit` feature.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Once};

use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage, v0, v1};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
pub use tempfile::TempDir;
use tracing_subscriber::{EnvFilter, fmt};
use v42_calculator_interface::builder::Expr as E;

pub use v42_calculator_interface::ID as V42_ID;

use crate::{Slot, ledger::Block, runtime::TransactionView};

static TRACING: Once = Once::new();

/// Standard transaction wire format produced by client SDKs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireVersion {
    /// Unversioned legacy message.
    Legacy,
    /// Version 0 message without address lookup tables.
    V0,
    /// Version 1 message.
    V1,
}

/// Installs a libtest-aware tracing subscriber for test processes.
///
/// The default filter is intentionally quiet. Set `RUST_LOG` and run tests with
/// `-- --nocapture` to see lower-level spans and events while debugging.
pub fn init_tracing() {
    TRACING.call_once(|| {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
        let _ = fmt().with_env_filter(filter).with_test_writer().try_init();
    });
}

/// A throwaway on-disk directory; the returned guard must outlive the store
/// opened over it, since these stores keep their files open/mmapped.
pub fn tempdir() -> TempDir {
    TempDir::new().unwrap()
}

/// A block boundary with a distinct hash and time derived from `slot`.
pub fn block(slot: Slot) -> Block {
    let mut hash = [0; 32];
    let bytes = slot.to_le_bytes();
    hash[..bytes.len()].copy_from_slice(&bytes);
    Block {
        slot,
        hash: Hash::new_from_array(hash),
        time: slot as i64,
        parent: Hash::default(),
    }
}

/// Signs `instructions` in a standard client wire format.
pub fn sign_versioned_instructions(
    payer: &Keypair,
    version: WireVersion,
    instructions: impl AsRef<[Instruction]>,
    blockhash: Hash,
) -> (Signature, Vec<u8>) {
    let instructions = instructions.as_ref();
    let message = match version {
        WireVersion::Legacy => VersionedMessage::Legacy(Message::new_with_blockhash(
            instructions,
            Some(&payer.pubkey()),
            &blockhash,
        )),
        WireVersion::V0 => VersionedMessage::V0(
            v0::Message::try_compile(&payer.pubkey(), instructions, &[], blockhash).unwrap(),
        ),
        WireVersion::V1 => VersionedMessage::V1(
            v1::Message::try_compile(&payer.pubkey(), instructions, blockhash).unwrap(),
        ),
    };
    let transaction = VersionedTransaction::try_new(message, &[payer]).unwrap();
    let signature = transaction.signatures[0];
    (signature, wincode::serialize(&transaction).unwrap())
}

/// Signs `instructions` from `payer` against `blockhash`, returning the first
/// signature and the wincode-serialized transaction bytes.
pub fn sign_instructions(
    payer: &Keypair,
    instructions: impl AsRef<[Instruction]>,
    blockhash: Hash,
) -> (Signature, Arc<Vec<u8>>) {
    let (signature, transaction) =
        sign_versioned_instructions(payer, WireVersion::Legacy, instructions, blockhash);
    (signature, Arc::new(transaction))
}

/// Builds one v42 instruction that sums every supplied operand into `output`.
pub fn v42_sum(output: Pubkey, operands: &[Pubkey]) -> Instruction {
    assert!(
        !operands.is_empty(),
        "v42 sum requires at least one operand"
    );
    let expression = (2..=operands.len()).fold(E::acc(1), |expr, index| expr + E::acc(index as u8));
    expression.compose(output, operands)
}

/// Builds a v42 instruction retaining `value` while adding evaluator work.
pub fn v42_padded_value(output: Pubkey, value: i64, terms: usize) -> Instruction {
    let expression = (1..terms).fold(E::lit(value), |expr, _| expr + E::lit(0));
    expression.compose(output, &[])
}

/// Returns deterministic non-uniform bytes for detecting damaged large payloads.
pub fn patterned_bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|index| seed.wrapping_add((index % 251) as u8)).collect()
}

/// Signs `instructions` and returns the sanitized transaction view consumed by
/// the runtime.
pub fn signed_view(
    payer: &Keypair,
    instructions: impl AsRef<[Instruction]>,
    blockhash: Hash,
) -> (Signature, TransactionView) {
    let (signature, bytes) = sign_instructions(payer, instructions, blockhash);
    let view = TransactionView::try_new_sanitized(bytes, true).unwrap();
    (signature, view)
}

/// A signed, wincode-serialized v42 transaction plus its first signature.
///
/// The instruction references `accounts` as read-only keys so they land in the
/// transaction's static account keys (and thus any account index), and targets
/// the v42 program so the transaction is executable, not just well-formed. A
/// fresh random payer per call keeps signatures unique without varying the
/// blockhash.
pub fn transaction(accounts: &[Pubkey]) -> (Signature, Arc<Vec<u8>>) {
    let payer = Keypair::new();
    let metas = accounts.iter().map(|k| AccountMeta::new_readonly(*k, false)).collect();
    let ix = Instruction::new_with_bytes(V42_ID, &[], metas);
    sign_instructions(&payer, [ix], Hash::default())
}
