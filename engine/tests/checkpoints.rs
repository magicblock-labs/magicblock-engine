//! Checksum checkpoint scheduling through the production pacemaker and ledger.

use engine::testkit::{Pacing, TestEngine};
use keeper::testkit::{Dirs, keeper_builder};
use ledger::{
    request::{ReadRequest, ReplayParams, RequestPayload},
    schema::OwnedBlockstoreEntry,
};
use tokio::sync::mpsc;

/// Proves checkpoint intervals are independent and opt-in, share full-seal
/// boundaries without duplication, and leave snapshot metadata untouched.
#[tokio::test(flavor = "multi_thread")]
async fn checkpoint_intervals_preserve_superblock_boundaries() {
    // The non-dividing intervals exercise independent checkpoints at 2 and 4,
    // a full seal alone at 3, and a shared boundary at 6 in one engine.
    let cases: &[(u64, u64, &[u64])] = &[(3, 0, &[]), (3, 2, &[2, 4]), (0, 2, &[2, 4, 6])];
    for &(superblock, checkpoint, expected) in cases {
        let dirs = Dirs::default();
        let mut builder = keeper_builder(&dirs);
        builder.blockstore.superblock = superblock;
        builder.blockstore.checkpoint = checkpoint;
        let mut engine = TestEngine::from_builder(dirs, builder, Pacing::External).await;
        for slot in 1u64..=6 {
            let sealed = engine.superblocks().sealed();
            let head = engine.ledger().head();
            engine.advance(1).await;
            engine.sync().await;
            if expected.contains(&slot) {
                assert_eq!(engine.superblocks().sealed(), sealed);
                assert_eq!(engine.ledger().head(), head);
            }
        }

        let (tx, mut rx) = mpsc::channel(4);
        let (request, completion) = RequestPayload::new(ReplayParams { superblock: 0, tx });
        engine.ledger().reader.send_async(ReadRequest::Replay(request)).await.unwrap();
        let mut slot = 0;
        let mut checkpoints = Vec::new();
        let mut seals = Vec::new();
        while let Some(entry) = rx.recv().await {
            match entry {
                OwnedBlockstoreEntry::Block(block) => slot = block.slot,
                OwnedBlockstoreEntry::Checkpoint(_) => checkpoints.push(slot),
                OwnedBlockstoreEntry::Superblock(_) => seals.push(slot),
                _ => {}
            }
        }
        completion.recv().await.unwrap().unwrap();
        assert_eq!(checkpoints, expected);
        assert_eq!(seals, if superblock == 0 { &[][..] } else { &[3, 6][..] });
        engine.close().await;
    }
}
