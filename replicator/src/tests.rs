//! Signed boundary authentication through the real replication ingestion path.

use std::{
    io::{BufReader, Write},
    net::{TcpListener, TcpStream},
    time::Duration,
};

use engine::{Engine, testkit::TestEngine};
use ledger::schema::{Block, Checkpoint, OwnedBlockstoreEntry, Reset, Signed, SuperblockSeal};
use solana_keypair::Keypair;

use crate::{
    ReplicationError, Result,
    client::{Ingest, IngestExit, ReplicationMessage},
};

/// Sends one encoded record through real ingestion and waits for its outcome.
fn ingest(
    engine: &Engine,
    bytes: &[u8],
    case: &str,
) -> (Option<ReplicationMessage>, Result<IngestExit>) {
    let timeout = Duration::from_secs(4);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let mut writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (reader, _) = listener.accept().unwrap();
    reader.set_read_timeout(Some(timeout)).unwrap();
    writer.set_write_timeout(Some(timeout)).unwrap();
    let (rx, worker) = Ingest::spawn(
        BufReader::new(reader),
        engine.verifier(),
        engine.authority(),
    )
    .unwrap();
    writer.write_all(bytes).unwrap();
    drop(writer);
    let message = match rx.recv_timeout(timeout) {
        Ok(message) => Some(message),
        Err(flume::RecvTimeoutError::Disconnected) => None,
        Err(flume::RecvTimeoutError::Timeout) => panic!("{case}: ingestion timed out"),
    };
    drop(rx);
    (
        message,
        worker.join().unwrap_or_else(|_| panic!("{case}: ingestion panicked")),
    )
}

/// Proves ingestion preserves valid boundary records and rejects tampered
/// payloads and wrong-key signatures for blocks, seals, resets, and checkpoints.
#[tokio::test(flavor = "multi_thread")]
async fn authenticates_boundary_records() {
    let engine = TestEngine::new().await;
    let other = Keypair::new();
    for (scenario, signer, tamper) in [
        ("valid", engine.signer(), 0),
        ("tampered", engine.signer(), 1),
        ("wrong signer", &other, 0),
    ] {
        let mut block = Signed::new(Block::new(1, 100), signer);
        let mut seal = Signed::new(
            SuperblockSeal {
                id: 1,
                checksum: 2,
                transactions: 3,
            },
            signer,
        );
        let mut reset = Signed::new(Reset(1), signer);
        let mut checkpoint = Signed::new(Checkpoint(2), signer);
        // Change payloads after signing, independently for each scenario.
        block.payload.slot += tamper;
        seal.payload.checksum += tamper;
        reset.payload.0 += tamper;
        checkpoint.payload.0 += tamper;
        for (kind, record) in [
            ("block", OwnedBlockstoreEntry::Block(block)),
            ("superblock", OwnedBlockstoreEntry::Superblock(seal)),
            ("reset", OwnedBlockstoreEntry::Reset(reset)),
            ("checkpoint", OwnedBlockstoreEntry::Checkpoint(checkpoint)),
        ] {
            let case = format!("{kind}, {scenario}");
            let bytes = wincode::serialize(&record).unwrap();
            let (message, result) = ingest(&engine, &bytes, &case);
            if scenario == "valid" {
                let accepted = match message {
                    Some(ReplicationMessage::Block(block)) => OwnedBlockstoreEntry::Block(block),
                    Some(ReplicationMessage::Superblock(seal)) => {
                        OwnedBlockstoreEntry::Superblock(seal)
                    }
                    Some(ReplicationMessage::Reset(reset)) => OwnedBlockstoreEntry::Reset(reset),
                    Some(ReplicationMessage::Checkpoint(checkpoint)) => {
                        OwnedBlockstoreEntry::Checkpoint(checkpoint)
                    }
                    _ => panic!("{case}: expected a boundary record"),
                };
                // Compare the whole encoded record, including the original signature.
                assert_eq!(wincode::serialize(&accepted).unwrap(), bytes, "{case}");
                assert!(matches!(result, Ok(IngestExit::Disconnected(_))), "{case}");
            } else {
                assert!(message.is_none(), "{case}: invalid record reached Control");
                assert!(
                    matches!(result,
                        Err(ReplicationError::InvalidSignature(actual)) if actual == kind
                    ),
                    "{case}: expected signature rejection"
                );
            }
        }
    }
    engine.close().await;
}
