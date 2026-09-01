#![allow(unsafe_op_in_unsafe_fn)]

use std::{collections::BTreeSet, ops::Range};

use heed::Result;
use solana_account::BorrowedAccount;
use tracing::info;

use crate::{
    metrics::{self, Operation},
    store::kv::{Offset, OwnerAndOffset},
};

use super::PersistedStore;

/// Smallest useful destination remainder, in 8-byte storage units.
pub(crate) const MIN_REMAINDER: u64 = 43;
type Fit = (u64, Offset, usize);

/// Result of one committed packing pass.
pub(crate) struct Defragged {
    pub(crate) moved: usize,
    pub(crate) reclaimed: u64,
}

impl Defragged {
    pub(crate) fn changed(&self) -> bool {
        self.moved > 0 || self.reclaimed > 0
    }
}

/// Free span in the persisted image file, measured in storage units.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Hole {
    offset: Offset,
    units: u64,
}

impl Hole {
    fn new((units, offset): (u64, Offset)) -> Self {
        Self { offset, units }
    }

    fn end(self) -> Offset {
        self.offset + self.units
    }
}

/// Adjacent entry-time holes treated as one packing destination.
struct Run {
    parts: Range<usize>,
    offset: Offset,
    units: u64,
}

impl Run {
    fn take(&mut self, units: u64) -> Offset {
        debug_assert!(units <= self.units);
        let dst = self.offset;
        self.offset = self.offset + units;
        self.units -= units;
        dst
    }

    fn end(&self) -> Offset {
        Offset(self.offset.0 + self.units)
    }
}

/// One account relocation planned against entry-time free space.
#[derive(Clone, Copy)]
struct Move {
    src: Offset,
    dst: Offset,
    units: u64,
}

impl Move {
    fn source(self) -> Hole {
        Hole {
            offset: self.src,
            units: self.units,
        }
    }
}

/// Temporary state for one non-overlapping packing pass.
struct Defrag<'a> {
    store: &'a PersistedStore,
    holes: Vec<Hole>,
    runs: Vec<Run>,
    moves: Vec<Move>,
    tail: Offset,
}

impl PersistedStore {
    /// Packs tail accounts into holes that existed at the start of this pass.
    ///
    /// Adjacent freelist entries form logical runs. An account uses an exact
    /// fit when available, otherwise the smallest run that leaves at least
    /// [`MIN_REMAINDER`] units. Destination remainders may accept more accounts
    /// in this pass; vacated source spans are deferred until a later pass. Some
    /// fragmented layouts therefore cannot progress.
    ///
    /// This operation is not crash-safe: interruption after publishing moved
    /// offsets can leave the active tree inconsistent and require a backup.
    ///
    /// # Safety
    ///
    /// No concurrent access may touch the persisted index or mapped storage
    /// while offsets are rewritten and bytes are moved.
    pub(crate) unsafe fn defragment(&self) -> Result<Defragged> {
        let _timer = metrics::time(Operation::Defragmentation);
        Defrag::new(self)?.execute()
    }
}

impl<'a> Defrag<'a> {
    /// Reads a consistent entry-time layout and plans tail-to-left moves.
    ///
    /// # Safety
    ///
    /// The store must be exclusively accessed, and indexed offsets must point
    /// to valid serialized accounts in its mapped storage.
    unsafe fn new(store: &'a PersistedStore) -> Result<Self> {
        let (mut holes, mut accounts) = {
            let txn = store.index.env.read_txn()?;
            let holes = store
                .index
                .freelist
                .iter(&txn)?
                .map(|r| r.map(Hole::new))
                .collect::<Result<Vec<_>>>()?;
            let accounts = if holes.is_empty() {
                Vec::new()
            } else {
                store
                    .index
                    .accounts
                    .iter(&txn)?
                    .map(|r| r.map(|(_, data)| data.offset))
                    .collect::<Result<Vec<_>>>()?
            };
            (holes, accounts)
        };
        holes.sort_unstable();
        accounts.sort_unstable();

        let runs = Self::runs(&holes);
        let mut defrag = Self {
            store,
            holes,
            runs,
            moves: Vec::new(),
            tail: Offset(store.storage.cursor()),
        };
        defrag.pack(accounts.into_iter().rev());
        Ok(defrag)
    }

    /// Groups physically adjacent holes without changing their freelist shape.
    fn runs(holes: &[Hole]) -> Vec<Run> {
        let mut runs = Vec::new();
        let mut i = 0;
        while i < holes.len() {
            let first = i;
            let offset = holes[i].offset;
            let mut end = holes[i].end();
            i += 1;
            while let Some(hole) = holes.get(i)
                && hole.offset == end
            {
                end = hole.end();
                i += 1;
            }
            runs.push(Run {
                parts: first..i,
                offset,
                units: end - offset,
            });
        }
        runs
    }

    /// Selects the best exact fit or the best fit with a useful remainder.
    fn fit(fit: &BTreeSet<Fit>, units: u64) -> Option<Fit> {
        let &(largest, _, _) = fit.last()?;
        if units > largest {
            return None;
        }

        let low = (units, Offset(0), 0);
        let high = (units, Offset(u64::MAX), usize::MAX);
        if let Some(exact) = fit.range(low..=high).next() {
            return Some(*exact);
        }

        let minimum = units.checked_add(MIN_REMAINDER)?;
        if minimum > largest {
            return None;
        }
        fit.range((minimum, Offset(0), 0)..).next().copied()
    }

    /// Packs accounts in descending source order into eligible runs.
    ///
    /// # Safety
    ///
    /// Every supplied offset must point to a valid serialized account, and no
    /// concurrent access may modify the index, freelist, or mapped storage.
    unsafe fn pack(&mut self, accounts: impl Iterator<Item = Offset>) {
        // Best fit by remaining units, then by the lowest current offset.
        let mut fit: BTreeSet<Fit> = self
            .runs
            .iter()
            .enumerate()
            .map(|(i, run)| (run.units, run.offset, i))
            .collect();
        let mut eligible = self.runs.len();

        for src in accounts {
            // Runs are already ordered by their physical end.
            while eligible > 0 && self.runs[eligible - 1].end() > src {
                let i = eligible - 1;
                fit.remove(&(self.runs[i].units, self.runs[i].offset, i));
                eligible -= 1;
            }
            if fit.is_empty() {
                break;
            }

            let units = u64::from(BorrowedAccount::span(self.store.storage.at(src)));
            let Some((remaining, start, i)) = Self::fit(&fit, units) else {
                continue;
            };
            fit.remove(&(remaining, start, i));
            let dst = self.runs[i].take(units);
            self.moves.push(Move { src, dst, units });
            let run = &self.runs[i];
            if run.units > 0 {
                fit.insert((run.units, run.offset, i));
            }
        }
    }

    /// Returns the first unit in the final free suffix without re-sorting it.
    fn compacted_tail(&self) -> Offset {
        let mut run = self.runs.len();
        let mut movement = 0;
        let mut tail = self.tail;

        loop {
            while run > 0 && self.runs[run - 1].units == 0 {
                run -= 1;
            }
            let free = (run > 0).then(|| {
                let run = &self.runs[run - 1];
                (run.offset, run.end())
            });
            let source = self.moves.get(movement).copied().map(Move::source);
            let (offset, end, from_run) = match (free, source) {
                (Some((offset, end)), Some(source)) if offset >= source.offset => {
                    (offset, end, true)
                }
                (Some(_), Some(source)) => (source.offset, source.end(), false),
                (Some((offset, end)), None) => (offset, end, true),
                (None, Some(source)) => (source.offset, source.end(), false),
                (None, None) => break,
            };
            if end != tail {
                break;
            }
            tail = offset;
            if from_run {
                run -= 1;
            } else {
                movement += 1;
            }
        }
        tail
    }

    /// Copies the plan and publishes all index and freelist changes.
    ///
    /// # Safety
    ///
    /// The entry-time layout must remain unchanged since planning, and no
    /// concurrent access may observe or modify storage while moves publish.
    unsafe fn execute(self) -> Result<Defragged> {
        let tail = self.compacted_tail();
        let outcome = Defragged {
            moved: self.moves.len(),
            reclaimed: self.tail - tail,
        };
        if !outcome.changed() {
            info!("nothing to defragment");
            return Ok(outcome);
        }

        // Entry-time destinations are disjoint, so every source remains intact
        // until the complete plan has been copied.
        for movement in &self.moves {
            self.store.storage.at(movement.src).copy_to_nonoverlapping(
                self.store.storage.at(movement.dst),
                movement.units as usize,
            );
        }

        let mut txn = self.store.index.env.write_txn()?;
        for movement in &self.moves {
            let ptr = self.store.storage.at(movement.src);
            let pubkey = BorrowedAccount::pubkey(ptr);
            let owner = BorrowedAccount::init(ptr).owner().into();
            let data = OwnerAndOffset { owner, offset: movement.dst };
            self.store.index.relocate(&pubkey, movement.src, data, &mut txn)?;
        }
        self.publish(tail, &mut txn)?;
        txn.commit()?;

        self.store.storage.stats().compact(outcome.moved);
        if outcome.reclaimed > 0 {
            self.store.storage.shrink(tail.0)?;
        }
        info!(
            moved = outcome.moved,
            reclaimed = outcome.reclaimed,
            "defragmented persisted storage"
        );
        Ok(outcome)
    }

    /// Publishes final free spans while retaining untouched component sizes.
    fn publish(&self, tail: Offset, txn: &mut heed::RwTxn<'_>) -> Result<()> {
        for run in &self.runs {
            for &hole in &self.holes[run.parts.clone()] {
                let offset = hole.offset.max(run.offset);
                let end = hole.end().min(tail);
                if offset == hole.offset && end == hole.end() {
                    continue;
                }
                self.store.index.freelist.delete_one_duplicate(txn, &hole.units, &hole.offset)?;
                if offset < end {
                    let units = end - offset;
                    self.store.index.freelist.put(txn, &units, &offset)?;
                }
            }
        }
        for movement in &self.moves {
            if movement.src < tail {
                let end = movement.source().end().min(tail);
                let units = end - movement.src;
                self.store.index.freelist.put(txn, &units, &movement.src)?;
            }
        }
        Ok(())
    }
}
