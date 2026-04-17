#![allow(unsafe_op_in_unsafe_fn)]

use heed::Result;
use solana_account::BorrowedAccount;
use tracing::{debug, info};

use crate::{
    metrics::{self, Operation},
    store::kv::{Offset, OwnerAndOffset},
};

use super::PersistedStore;

/// Free span in the persisted image file, measured in storage units and
/// ordered by offset for the sweep.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Hole {
    offset: Offset,
    units: u32,
}

impl Hole {
    /// Builds one hole from the freelist entry tuple.
    fn new((units, offset): (u32, Offset)) -> Self {
        Self { offset, units }
    }

    /// Returns the first offset past the hole.
    fn end(&self) -> Offset {
        self.offset + self.units
    }
}

impl PersistedStore {
    /// Compacts the tail half of the freelist and repacks the storage.
    ///
    /// Caller must guarantee exclusive access while defrag runs. Holes are
    /// swept in file order, contiguous holes are merged into one run, and the
    /// following live span is reindexed before its bytes are slid left. Each
    /// pass starts at the tail half of the sorted hole list so repeated runs
    /// halve the remaining work.
    ///
    /// # Safety
    ///
    /// No concurrent mutation may touch the persisted index or mapped
    /// storage while offsets are rewritten and bytes are moved.
    pub(crate) unsafe fn defragment(&self) -> Result<()> {
        let _timer = metrics::time(Operation::Defragmentation);
        let mut holes = {
            let txn = self.index.env.read_txn()?;
            self.index
                .freelist
                .iter(&txn)?
                .map(|r| r.map(Hole::new))
                .collect::<Result<Vec<_>>>()?
        };
        holes.sort_unstable();

        let mut i = holes.len() / 2;
        // `dst` is the next write head in storage units.
        let Some(mut dst) = holes.get(i).map(|h| h.offset) else {
            debug!("nothing to defragment");
            return Ok(());
        };
        // Tail is the live end; if the last hole reaches it, there is no
        // trailing live span to move.
        let tail = Offset(self.storage.cursor());
        while i < holes.len() {
            let run = i;
            let mut end = holes[i].end();
            i += 1;

            while let Some(&hole) = holes.get(i) {
                if hole.offset != end {
                    break;
                }
                end = hole.end();
                i += 1;
            }

            let next = holes.get(i).map(|h| h.offset).unwrap_or(tail);
            // Defrag runs exclusively, so rewrite and commit the index before
            // moving the live bytes that now point at the future offsets.
            let shift = end - dst;
            self.reindex(end, next, shift, &holes[run..i])?;
            // Source and destination overlap by design during left-compaction.
            dst = self.slide(end, dst, next - end);
        }
        let reclaimed = tail - dst;
        info!(reclaimed, "defragmented persisted storage");
        self.storage.shrink(dst.0).map_err(Into::into)
    }

    /// Rewrites the index for a live span before its bytes are moved.
    ///
    /// # Safety
    ///
    /// `src..end` must cover whole serialized accounts in mapped storage.
    /// The caller must guarantee exclusive access while offsets are updated.
    unsafe fn reindex(&self, src: Offset, end: Offset, shift: u32, run: &[Hole]) -> Result<()> {
        let mut txn = self.index.env.write_txn()?;
        let mut pos = src;
        while pos < end {
            let ptr = self.storage.at(pos);
            let span = BorrowedAccount::span(ptr);
            let next = pos + span;
            // The bytes are still at `pos`; compute the future offset before copying.
            let pubkey = BorrowedAccount::pubkey(ptr);
            let account = BorrowedAccount::init(ptr);
            let owner = account.owner().into();
            let data = OwnerAndOffset { owner, offset: pos - shift };
            self.index.relocate(&pubkey, pos, data, &mut txn)?;
            self.storage.stats().compact();
            pos = next;
        }
        for hole in run {
            self.index.freelist.delete_one_duplicate(&mut txn, &hole.units, &hole.offset)?;
        }
        txn.commit()
    }

    /// Slides a live region left after the index has been committed.
    ///
    /// # Safety
    ///
    /// `src..src+len` and `dst..dst+len` must be valid mapped storage
    /// ranges. The ranges may overlap.
    unsafe fn slide(&self, src: Offset, dst: Offset, len: u32) -> Offset {
        let src = self.storage.at(src);
        src.copy_to(self.storage.at(dst), len as usize);
        dst + len
    }
}
