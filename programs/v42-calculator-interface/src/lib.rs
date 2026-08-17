#![doc = include_str!("../README.md")]

use solana_pubkey::declare_id;

pub mod opcodes;

#[cfg(feature = "builder")]
pub mod builder;

declare_id!("V42CaLcu1atormagicb1ock11111111111111111111");
