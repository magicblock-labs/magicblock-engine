#![doc = include_str!("../README.md")]

use solana_account::{AccountFieldPatch, OwnedAccount, StateFlags};
use solana_instruction::{AccountMeta, Instruction, error::InstructionError};
use solana_pubkey::{Pubkey, declare_id};
use wincode::{SchemaRead, SchemaWrite};

declare_id!("MagicRootDRJ5atQjSJUxFjXzjeZXMADHUDznbk22gy");

/// Trusted provenance and follow-up instructions for a finalized account.
///
/// `source_program` becomes the effective caller of each direct action. Code
/// constructing this payload must therefore verify it against authoritative
/// provenance, such as the owner in a slot-matched delegation record, before
/// submitting [`MagicRootInstruction::PostFinalize`]. It must not be copied
/// from untrusted action data. The source program need not equal the finalized
/// account's current owner because projected accounts may deliberately retain
/// a different state owner.
#[derive(SchemaRead, SchemaWrite)]
pub struct PostFinalize {
    /// Verified program to expose as the effective caller of each direct action.
    pub source_program: Pubkey,
    /// Instructions to invoke after account finalization.
    pub actions: Vec<Instruction>,
}

/// Instructions accepted by the MagicRoot built-in program.
#[derive(SchemaRead, SchemaWrite)]
pub enum MagicRootInstruction {
    /// Apply one bounded account-image patch to the target account.
    Patch(AccountFieldPatch),
    /// Replace the target's complete flag value and, when executable, load it
    /// into the transaction's program cache. Does not change lamports.
    Finalize(StateFlags),
    /// Close the target account and hide any cached executable immediately.
    Delete,
    /// Run follow-up instructions immediately after finalizing the same target
    /// (e.g. initializing a freshly created account); each is invoked via CPI
    /// against the accounts it declares.
    PostFinalize(PostFinalize),
}

impl MagicRootInstruction {
    /// Composes this variant into an [`Instruction`] targeting the MagicRoot
    /// program: prepends the target `account` meta, appends any metas the
    /// variant requires, and serializes the instruction data.
    pub fn compose(&self, account: Pubkey) -> Result<Instruction, InstructionError> {
        let mut accounts = vec![AccountMeta::new(account, false)];
        self.extend_metas(&mut accounts);
        // NOTE this code can never error, wincode serialization for instruction is infallible
        let data = wincode::serialize(self).map_err(|_| InstructionError::BorshIoError)?;
        Ok(Instruction { program_id: ID, accounts, data })
    }

    /// Composes `account` into the ordered instruction sequence that patches
    /// every non-flag field on `target`, then finalizes it with `account`'s
    /// complete flag value.
    pub fn compose_account(
        target: Pubkey,
        account: OwnedAccount,
    ) -> Result<Vec<Instruction>, InstructionError> {
        let flags = account.flags();
        let patches = AccountFieldPatch::sequence(account);
        let mut instructions = Vec::with_capacity(patches.len() + 1);
        for patch in patches {
            instructions.push(Self::Patch(patch).compose(target)?);
        }
        instructions.push(Self::Finalize(flags).compose(target)?);
        Ok(instructions)
    }

    /// Appends the extra account metas a variant requires. Only [`PostFinalize`]
    /// contributes any: the program id and de-signed accounts of each follow-up
    /// instruction.
    ///
    /// [`PostFinalize`]: MagicRootInstruction::PostFinalize
    fn extend_metas(&self, accounts: &mut Vec<AccountMeta>) {
        let Self::PostFinalize(post_finalize) = self else {
            return;
        };
        for ix in &post_finalize.actions {
            accounts.push(AccountMeta::new_readonly(ix.program_id, false));
            let metas = ix.accounts.clone().into_iter().map(|mut meta| {
                meta.is_signer = false;
                meta
            });
            accounts.extend(metas)
        }
    }
}
