use {
    crate::{
        bytes::{advance_offset_for_array, read_compressed_u16, try_u32_offset},
        result::{Result, TransactionViewError},
    },
    solana_pubkey::Pubkey,
};

pub(crate) const MAX_LEGACY_OR_V0_ACCOUNT_KEYS: u16 = u8::MAX as u16;

/// Contains metadata about the static account keys in a transaction packet.
#[derive(Debug, Default)]
pub(crate) struct StaticAccountKeysFrame {
    /// The number of static accounts in the transaction.
    pub(crate) num_static_accounts: u8,
    /// The offset to the first static account in the transaction.
    pub(crate) offset: u32,
}

impl StaticAccountKeysFrame {
    #[inline(always)]
    pub(crate) fn try_new(bytes: &[u8], offset: &mut usize) -> Result<Self> {
        let num_static_accounts = read_compressed_u16(bytes, offset)?;
        if num_static_accounts == 0 || num_static_accounts > MAX_LEGACY_OR_V0_ACCOUNT_KEYS {
            return Err(TransactionViewError::ParseError);
        }

        let static_accounts_offset = try_u32_offset(*offset)?;
        advance_offset_for_array::<Pubkey>(bytes, offset, num_static_accounts)?;

        Ok(Self {
            num_static_accounts: num_static_accounts as u8,
            offset: static_accounts_offset,
        })
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_short_vec::ShortVec};

    #[test]
    fn test_zero_accounts() {
        let bytes = bincode::serialize(&ShortVec(Vec::<Pubkey>::new())).unwrap();
        let mut offset = 0;
        assert!(StaticAccountKeysFrame::try_new(&bytes, &mut offset).is_err());
    }

    #[test]
    fn test_one_account() {
        let bytes = bincode::serialize(&ShortVec(vec![Pubkey::default()])).unwrap();
        let mut offset = 0;
        let frame = StaticAccountKeysFrame::try_new(&bytes, &mut offset).unwrap();
        assert_eq!(frame.num_static_accounts, 1);
        assert_eq!(frame.offset, 1);
        assert_eq!(offset, 1 + core::mem::size_of::<Pubkey>());
    }

    #[test]
    fn test_max_accounts() {
        let accounts = vec![Pubkey::default(); 255];
        let bytes = bincode::serialize(&ShortVec(accounts)).unwrap();
        let mut offset = 0;
        let frame = StaticAccountKeysFrame::try_new(&bytes, &mut offset).unwrap();
        assert_eq!(frame.num_static_accounts, 255);
        assert_eq!(frame.offset, 2);
        assert_eq!(offset, bytes.len());
    }

    #[test]
    fn test_too_many_accounts() {
        let accounts = vec![Pubkey::default(); 256];
        let bytes = bincode::serialize(&ShortVec(accounts)).unwrap();
        let mut offset = 0;
        assert!(StaticAccountKeysFrame::try_new(&bytes, &mut offset).is_err());
    }

    #[test]
    fn test_u16_max_accounts() {
        let signatures = vec![Pubkey::default(); u16::MAX as usize];
        let bytes = bincode::serialize(&ShortVec(signatures)).unwrap();
        let mut offset = 0;
        assert!(StaticAccountKeysFrame::try_new(&bytes, &mut offset).is_err());
    }
}
