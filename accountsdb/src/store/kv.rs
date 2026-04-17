use std::{array, borrow::Cow, ops};

use bytemuck::{Pod, Zeroable};
use heed::{BoxedError, BytesDecode, BytesEncode, byteorder::LittleEndian, types::U32};
use solana_pubkey::Pubkey;

/// Result type used by LMDB byte codecs.
pub(crate) type CodecResult<T> = Result<T, BoxedError>;
/// Little-endian `u32` value stored in the freelist.
pub(super) type U32LE = U32<LittleEndian>;
/// Offset into mapped storage, measured in storage units.
#[derive(Clone, Copy, Zeroable, Pod, PartialEq, Eq, PartialOrd, Ord)]
#[repr(C)]
pub(crate) struct Offset(pub(super) u32);

/// Compact 16-byte LMDB tag derived from the tail half of a pubkey.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub(crate) struct KeyTail([u8; 16]);

impl From<Pubkey> for KeyTail {
    fn from(v: Pubkey) -> Self {
        Self(array::from_fn(|i| v.as_array()[i + size_of::<Self>()]))
    }
}

/// Full 32-byte pubkey codec for the accounts table.
pub(super) struct PubkeyBytes;

/// LMDB value for the accounts table.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub(crate) struct OwnerAndOffset {
    /// Owner key tag for the stored account image.
    pub(crate) owner: KeyTail,
    /// Offset into mapped storage.
    pub(crate) offset: Offset,
}

impl<'a> BytesEncode<'a> for KeyTail {
    type EItem = Self;

    fn bytes_encode(item: &'a Self::EItem) -> CodecResult<Cow<'a, [u8]>> {
        Ok(bytemuck::bytes_of(item).into())
    }
}

impl<'a> BytesDecode<'a> for KeyTail {
    type DItem = &'a Self;

    fn bytes_decode(bytes: &'a [u8]) -> CodecResult<Self::DItem> {
        bytemuck::try_from_bytes(bytes).map_err(Into::into)
    }
}

impl<'a> BytesEncode<'a> for PubkeyBytes {
    type EItem = Pubkey;

    fn bytes_encode(item: &'a Self::EItem) -> CodecResult<Cow<'a, [u8]>> {
        Ok(item.as_array().into())
    }
}

impl<'a> BytesDecode<'a> for PubkeyBytes {
    type DItem = &'a Pubkey;

    fn bytes_decode(bytes: &'a [u8]) -> CodecResult<Self::DItem> {
        bytemuck::try_from_bytes(bytes).map_err(Into::into)
    }
}

impl<'a> BytesEncode<'a> for OwnerAndOffset {
    type EItem = Self;

    fn bytes_encode(item: &'a Self::EItem) -> CodecResult<Cow<'a, [u8]>> {
        Ok(bytemuck::bytes_of(item).into())
    }
}

impl<'a> BytesDecode<'a> for OwnerAndOffset {
    type DItem = Self;

    fn bytes_decode(bytes: &'a [u8]) -> CodecResult<Self::DItem> {
        bytemuck::try_pod_read_unaligned(bytes).map_err(Into::into)
    }
}

impl<'a> BytesEncode<'a> for Offset {
    type EItem = Self;

    fn bytes_encode(item: &'a Self::EItem) -> CodecResult<Cow<'a, [u8]>> {
        U32LE::bytes_encode(&item.0)
    }
}

impl<'a> BytesDecode<'a> for Offset {
    type DItem = Self;

    fn bytes_decode(bytes: &'a [u8]) -> CodecResult<Self::DItem> {
        U32LE::bytes_decode(bytes).map(Self)
    }
}

impl ops::Add<u32> for Offset {
    type Output = Self;
    fn add(self, rhs: u32) -> Self::Output {
        Self(self.0 + rhs)
    }
}

impl ops::Sub<u32> for Offset {
    type Output = Self;
    fn sub(self, rhs: u32) -> Self::Output {
        Self(self.0 - rhs)
    }
}

impl ops::Sub for Offset {
    type Output = u32;
    fn sub(self, rhs: Self) -> Self::Output {
        self.0 - rhs.0
    }
}
