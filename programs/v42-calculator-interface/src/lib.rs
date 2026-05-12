#![doc = include_str!("../README.md")]

use solana_pubkey::declare_id;

pub mod opcodes;

#[cfg(feature = "builder")]
pub mod builder;

declare_id!("V42CaLcu1atormagicb1ock11111111111111111111");

/// Transfer lamports and calculator value between instruction accounts 0 and
/// 1; an exact little-endian `i64` delta follows.
pub const TRANSFER: u8 = 0x30;
