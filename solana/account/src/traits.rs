use {
    crate::{
        Account, AccountSharedData,
        cow::{DirtyMarkers, StateFlags},
    },
    solana_account_info::debug_account_data::debug_account_data,
    solana_clock::Epoch,
    solana_instruction_error::LamportsError,
    solana_pubkey::Pubkey,
    std::{fmt, ops::Deref},
};

/// Read-only access to account state.
pub trait ReadableAccount: Sized {
    /// Returns the lamport balance.
    fn lamports(&self) -> u64;

    /// Returns the account data.
    fn data(&self) -> &[u8];

    /// Returns the account owner.
    fn owner(&self) -> &Pubkey;

    /// Returns whether the account is executable.
    fn executable(&self) -> bool;

    /// Returns the rent epoch view for this account.
    fn rent_epoch(&self) -> Epoch;
}

/// Writable access to account state.
pub trait WritableAccount: ReadableAccount {
    /// Replaces the lamport balance.
    fn set_lamports(&mut self, lamports: u64);

    /// Adds lamports or returns an overflow error.
    fn checked_add_lamports(&mut self, lamports: u64) -> Result<(), LamportsError> {
        self.set_lamports(
            self.lamports().checked_add(lamports).ok_or(LamportsError::ArithmeticOverflow)?,
        );
        Ok(())
    }

    /// Subtracts lamports or returns an underflow error.
    fn checked_sub_lamports(&mut self, lamports: u64) -> Result<(), LamportsError> {
        self.set_lamports(
            self.lamports()
                .checked_sub(lamports)
                .ok_or(LamportsError::ArithmeticUnderflow)?,
        );
        Ok(())
    }

    /// Adds lamports and saturates on overflow.
    fn saturating_add_lamports(&mut self, lamports: u64) {
        self.set_lamports(self.lamports().saturating_add(lamports))
    }

    /// Subtracts lamports and saturates on underflow.
    fn saturating_sub_lamports(&mut self, lamports: u64) {
        self.set_lamports(self.lamports().saturating_sub(lamports))
    }

    /// Returns mutable access to the account data.
    fn data_as_mut_slice(&mut self) -> &mut [u8];

    /// Replaces the owner.
    fn set_owner(&mut self, owner: Pubkey);

    /// Copies 32 raw bytes into the owner pubkey.
    fn copy_into_owner_from_slice(&mut self, source: &[u8]);

    /// Sets the executable flag.
    fn set_executable(&mut self, executable: bool);

    /// Sets the rent epoch view if the implementation stores one.
    ///
    /// Implementations that do not store rent epoch may ignore this.
    fn set_rent_epoch(&mut self, epoch: Epoch);
}

/// Returns `true` when the readable account fields match.
///
/// This ignores storage form and any non-readable metadata.
pub fn accounts_equal<T: ReadableAccount, U: ReadableAccount>(me: &T, other: &U) -> bool {
    me.lamports() == other.lamports()
        && me.executable() == other.executable()
        && me.rent_epoch() == other.rent_epoch()
        && me.owner() == other.owner()
        && me.data() == other.data()
}

/// Formats readable accounts with the same debug shape as `Account`.
pub(crate) fn debug_fmt<T: ReadableAccount>(
    item: &T,
    f: &mut fmt::Formatter<'_>,
    add: impl FnOnce(&mut fmt::DebugStruct<'_, '_>),
) -> fmt::Result {
    let mut f = f.debug_struct("Account");

    f.field("lamports", &item.lamports())
        .field("data.len", &item.data().len())
        .field("owner", &item.owner())
        .field("executable", &item.executable())
        .field("rent_epoch", &item.rent_epoch());
    add(&mut f);
    debug_account_data(item.data(), &mut f);

    f.finish()
}

impl<T> ReadableAccount for T
where
    T: Deref,
    T::Target: ReadableAccount,
{
    fn lamports(&self) -> u64 {
        self.deref().lamports()
    }

    fn data(&self) -> &[u8] {
        self.deref().data()
    }

    fn owner(&self) -> &Pubkey {
        self.deref().owner()
    }

    fn executable(&self) -> bool {
        self.deref().executable()
    }

    fn rent_epoch(&self) -> Epoch {
        self.deref().rent_epoch()
    }
}

impl ReadableAccount for Account {
    fn lamports(&self) -> u64 {
        self.lamports
    }

    fn data(&self) -> &[u8] {
        &self.data
    }

    fn owner(&self) -> &Pubkey {
        &self.owner
    }

    fn executable(&self) -> bool {
        self.executable
    }

    fn rent_epoch(&self) -> Epoch {
        self.rent_epoch
    }
}

impl WritableAccount for Account {
    fn set_lamports(&mut self, lamports: u64) {
        self.lamports = lamports;
    }

    fn data_as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.data
    }

    fn set_owner(&mut self, owner: Pubkey) {
        self.owner = owner;
    }

    fn copy_into_owner_from_slice(&mut self, source: &[u8]) {
        self.owner.as_mut().copy_from_slice(source);
    }

    fn set_executable(&mut self, executable: bool) {
        self.executable = executable;
    }

    fn set_rent_epoch(&mut self, epoch: Epoch) {
        self.rent_epoch = epoch;
    }
}

impl ReadableAccount for AccountSharedData {
    fn lamports(&self) -> u64 {
        self.lamports
    }

    fn data(&self) -> &[u8] {
        self.cow.data()
    }

    fn owner(&self) -> &Pubkey {
        &self.owner
    }

    fn executable(&self) -> bool {
        self.flags.contains(StateFlags::EXECUTABLE)
    }

    fn rent_epoch(&self) -> Epoch {
        Epoch::MAX
    }
}

impl WritableAccount for AccountSharedData {
    fn set_lamports(&mut self, lamports: u64) {
        if self.lamports == lamports {
            return;
        }
        self.translate();
        self.dirty.insert(DirtyMarkers::LAMPORTS);
        self.lamports = lamports;
    }

    fn data_as_mut_slice(&mut self) -> &mut [u8] {
        self.translate();
        self.mark_data_dirty();
        self.cow.data_mut()
    }

    fn set_owner(&mut self, owner: Pubkey) {
        if self.owner == owner {
            return;
        }
        self.translate();
        self.dirty.insert(DirtyMarkers::OWNER);
        self.owner = owner;
    }

    fn copy_into_owner_from_slice(&mut self, source: &[u8]) {
        self.translate();
        self.dirty.insert(DirtyMarkers::OWNER);
        self.owner.as_mut().copy_from_slice(source);
    }

    fn set_executable(&mut self, executable: bool) {
        self.set_flag(StateFlags::EXECUTABLE, executable);
    }

    fn set_rent_epoch(&mut self, _: Epoch) {}
}
