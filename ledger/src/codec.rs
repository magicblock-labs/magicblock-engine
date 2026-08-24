//! Execution-details compression codec.

use std::io;

use zstd::{
    bulk::{Compressor, Decompressor},
    zstd_safe::CParameter,
};

const COMPRESSION_LEVEL: i32 = 3;
const DICTIONARY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../assets/execution-details.dict"
));

/// Creates a reusable compressor for execution-details frames.
pub(crate) fn compressor() -> io::Result<Compressor<'static>> {
    let mut compressor = Compressor::with_dictionary(COMPRESSION_LEVEL, DICTIONARY)?;
    compressor.set_parameter(CParameter::DictIdFlag(false))?;
    Ok(compressor)
}

/// Creates a reusable decompressor for execution-details frames.
pub(crate) fn decompressor() -> io::Result<Decompressor<'static>> {
    Decompressor::with_dictionary(DICTIONARY)
}
