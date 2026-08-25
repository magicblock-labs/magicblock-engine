//! Execution-details codec tests.

use std::sync::Arc;

use bitcode::Buffer;
use zstd::zstd_safe::get_dict_id_from_frame;

use crate::{
    codec::{compressor, decompressor},
    schema::{Balances, CompiledInstruction, Cpis, ExecutionDetails, Instruction, ReturnData},
};

/// Proves frames omit dictionary IDs and matching Zstd/bitcode contexts round-trip.
#[test]
fn execution_details_dictionary_roundtrip() {
    let details = Some(ExecutionDetails {
        fee: 5_000,
        balances: Balances {
            pre: vec![10_000, 20_000],
            post: vec![9_000, 21_000],
        },
        logs: Arc::new(vec![
            "Program log: Instruction: Transfer".into(),
            "Program consumed 150 of 200000 compute units".into(),
        ]),
        cpi: Some(vec![Cpis(vec![Instruction {
            compiled: CompiledInstruction {
                program_index: 2,
                accounts: vec![0, 1],
                data: vec![3, 4, 5],
            },
            stack_height: 2,
        }])]),
        compute_units: 150,
        return_data: Some(ReturnData {
            program: [7; 32],
            data: Arc::new(vec![8, 9]),
        }),
    });

    let mut encoder = Buffer::new();
    let encoded = encoder.encode(&details).to_vec();
    let compressed = compressor().unwrap().compress(&encoded).unwrap();
    assert_eq!(get_dict_id_from_frame(&compressed), None);

    let decoded = decompressor().unwrap().decompress(&compressed, encoded.len()).unwrap();
    let mut decoder = Buffer::new();
    let details: Option<ExecutionDetails> = decoder.decode(&decoded).unwrap();
    let details = details.expect("execution details decoded");
    assert_eq!(details.fee, 5_000);
    assert_eq!(details.balances.post, [9_000, 21_000]);
    assert_eq!(details.logs.len(), 2);
    assert_eq!(details.cpi.unwrap()[0].0[0].stack_height, 2);
    assert_eq!(details.return_data.unwrap().data.as_slice(), &[8, 9]);
}
