#![doc = include_str!("../README.md")]

use solana_account::{AccountFieldPatch, OwnedAccount, StateFlags};
use solana_instruction::{AccountMeta, Instruction, error::InstructionError};
use solana_pubkey::{Pubkey, declare_id};
use wincode::{SchemaRead, SchemaWrite};

declare_id!("MagicRootDRJ5atQjSJUxFjXzjeZXMADHUDznbk22gy");

/// Instructions accepted by the MagicRoot built-in program.
#[derive(SchemaRead, SchemaWrite)]
pub enum MagicRootInstruction {
    /// Apply a single-field patch to the target account.
    Patch(AccountFieldPatch),
    /// Replace the target's complete flag value and, when executable, load it
    /// into the transaction's program cache. Does not change lamports.
    Finalize(StateFlags),
    /// Close the target account.
    Delete,
    /// Run follow-up instructions immediately after finalizing the same target
    /// (e.g. initializing a freshly created account); each is invoked via CPI
    /// against the accounts it declares.
    PostFinalize(Vec<Instruction>),
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
        let Self::PostFinalize(ixs) = self else {
            return;
        };
        for ix in ixs {
            accounts.push(AccountMeta::new_readonly(ix.program_id, false));
            let metas = ix.accounts.clone().into_iter().map(|mut meta| {
                meta.is_signer = false;
                meta
            });
            accounts.extend(metas)
        }
    }
}
