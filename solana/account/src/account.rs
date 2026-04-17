use {
    crate::{AccountSharedData, ReadableAccount, traits::debug_fmt},
    solana_account_info::AccountInfo,
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable},
    std::{cell::RefCell, fmt, rc::Rc},
};

/// An on-chain account with owned data and an explicit rent epoch.
#[repr(C)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize), serde(rename_all = "camelCase"))]
#[cfg_attr(feature = "wincode", derive(wincode::SchemaRead, wincode::SchemaWrite))]
#[derive(PartialEq, Eq, Clone, Default)]
pub struct Account {
    /// Lamports in the account.
    pub lamports: u64,
    /// Data held in the account.
    #[cfg_attr(feature = "serde", serde(with = "serde_bytes"))]
    pub data: Vec<u8>,
    /// The program that owns this account.
    pub owner: Pubkey,
    /// Whether the account contains executable program data.
    pub executable: bool,
    /// The epoch at which this account next owes rent.
    pub rent_epoch: Epoch,
}

#[cfg(feature = "serde")]
mod account_serialize {
    use {
        crate::ReadableAccount,
        serde::{Serialize, ser::Serializer},
        solana_clock::Epoch,
        solana_pubkey::Pubkey,
    };

    #[repr(C)]
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    /// Serialization shape shared by `Account` and `AccountSharedData`.
    struct Account<'a> {
        lamports: u64,
        #[serde(with = "serde_bytes")]
        data: &'a [u8],
        owner: &'a Pubkey,
        executable: bool,
        rent_epoch: Epoch,
    }

    /// Serializes any readable account using the canonical `Account` layout.
    pub(crate) fn serialize_account<S>(
        account: &impl ReadableAccount,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let account = Account {
            lamports: account.lamports(),
            data: account.data(),
            owner: account.owner(),
            executable: account.executable(),
            rent_epoch: account.rent_epoch(),
        };
        account.serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl serde::ser::Serialize for Account {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        account_serialize::serialize_account(self, serializer)
    }
}

#[cfg(feature = "serde")]
impl serde::ser::Serialize for AccountSharedData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        account_serialize::serialize_account(self, serializer)
    }
}

impl From<AccountSharedData> for Account {
    fn from(other: AccountSharedData) -> Self {
        Self {
            lamports: other.lamports(),
            data: other.data().to_vec(),
            owner: *other.owner(),
            executable: other.executable(),
            rent_epoch: other.rent_epoch(),
        }
    }
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_fmt(self, f)
    }
}

impl fmt::Debug for AccountSharedData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_fmt(self, f)
    }
}

impl Account {
    /// Builds an account from its exact field set.
    ///
    /// Used by the constructors to keep the owned layout in one place.
    fn from_parts(
        lamports: u64,
        data: Vec<u8>,
        owner: Pubkey,
        executable: bool,
        rent_epoch: Epoch,
    ) -> Self {
        Self {
            lamports,
            data,
            owner,
            executable,
            rent_epoch,
        }
    }

    /// Creates a new account with zero-filled data.
    pub fn new(lamports: u64, space: usize, owner: &Pubkey) -> Self {
        Self::new_rent_epoch(lamports, space, owner, Epoch::default())
    }

    /// Creates a new account wrapped in a `RefCell`.
    pub fn new_ref(lamports: u64, space: usize, owner: &Pubkey) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self::new(lamports, space, owner)))
    }

    /// Creates a new account whose data is the serialized state.
    #[cfg(feature = "bincode")]
    pub fn new_data<T: serde::Serialize>(
        lamports: u64,
        state: &T,
        owner: &Pubkey,
    ) -> Result<Self, bincode::Error> {
        let data = bincode::serialize(state)?;
        Ok(Self::from_parts(
            lamports,
            data,
            *owner,
            false,
            Epoch::default(),
        ))
    }

    /// Creates a new serialized account wrapped in a `RefCell`.
    #[cfg(feature = "bincode")]
    pub fn new_ref_data<T: serde::Serialize>(
        lamports: u64,
        state: &T,
        owner: &Pubkey,
    ) -> Result<RefCell<Self>, bincode::Error> {
        Self::new_data(lamports, state, owner).map(RefCell::new)
    }

    /// Creates a new account with fixed space and serialized state.
    #[cfg(feature = "bincode")]
    pub fn new_data_with_space<T: serde::Serialize>(
        lamports: u64,
        state: &T,
        space: usize,
        owner: &Pubkey,
    ) -> Result<Self, bincode::Error> {
        let mut account = Self::new(lamports, space, owner);
        crate::codec::serialize_data(&mut account, state)?;
        Ok(account)
    }

    /// Creates a new fixed-size serialized account wrapped in a `RefCell`.
    #[cfg(feature = "bincode")]
    pub fn new_ref_data_with_space<T: serde::Serialize>(
        lamports: u64,
        state: &T,
        space: usize,
        owner: &Pubkey,
    ) -> Result<RefCell<Self>, bincode::Error> {
        Self::new_data_with_space(lamports, state, space, owner).map(RefCell::new)
    }

    /// Creates a new account with an explicit rent epoch.
    pub fn new_rent_epoch(lamports: u64, space: usize, owner: &Pubkey, rent_epoch: Epoch) -> Self {
        Self::from_parts(lamports, vec![0; space], *owner, false, rent_epoch)
    }

    /// Deserializes the account data as `T`.
    #[cfg(feature = "bincode")]
    pub fn deserialize_data<T: serde::de::DeserializeOwned>(&self) -> Result<T, bincode::Error> {
        crate::codec::deserialize_data(self)
    }

    /// Serializes `state` into the existing account data buffer.
    #[cfg(feature = "bincode")]
    pub fn serialize_data<T: serde::Serialize>(&mut self, state: &T) -> Result<(), bincode::Error> {
        crate::codec::serialize_data(self, state)
    }
}

impl solana_account_info::Account for Account {
    fn get(&mut self) -> (&mut u64, &mut [u8], &Pubkey, bool) {
        (
            &mut self.lamports,
            &mut self.data,
            &self.owner,
            self.executable,
        )
    }
}

/// Builds `AccountInfo` values for accounts and signer bits.
///
/// The returned infos borrow the provided accounts directly.
pub fn create_is_signer_account_infos<'a>(
    accounts: &'a mut [(&'a Pubkey, bool, &'a mut Account)],
) -> Vec<AccountInfo<'a>> {
    accounts
        .iter_mut()
        .map(|(key, is_signer, account)| {
            AccountInfo::new(
                key,
                *is_signer,
                false,
                &mut account.lamports,
                &mut account.data,
                &account.owner,
                account.executable,
            )
        })
        .collect()
}

/// Owners that imply the account contains a loaded program.
pub const PROGRAM_OWNERS: &[Pubkey] =
    &[bpf_loader_upgradeable::id(), bpf_loader::id(), bpf_loader_deprecated::id()];
