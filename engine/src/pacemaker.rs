//! Block-boundary pacing.

use std::{num::NonZeroU64, time::Duration};

use derive_more::Deref;
use ledger::schema::Block;
use nucleus::{
    Slot,
    config::BlockstoreParams,
    runtime,
    shutdown::{Service, ShutdownHandle, ShutdownManager, ShutdownReason},
    unix_time,
};
use processor::{SequencerMessage, SimulatorMessage};
use tokio::{
    sync::mpsc::Receiver,
    time::{self, Interval, MissedTickBehavior},
};
use tracing::error;

use crate::{Engine, Result};

/// Channel used by external block producers.
pub type ExternalPacer = Receiver<ExternalBlock>;

/// Emits block boundaries into engine execution paths.
#[derive(Deref)]
pub struct PaceMaker {
    /// Engine handle used to submit each boundary.
    #[deref]
    engine: Engine,
    /// Source for the next block boundary.
    pacer: Pacer,
    /// Number of slots sealed into each superblock.
    superblock: NonZeroU64,
    /// Completion of the last queued seal, awaited before taking its successor.
    sealed: Option<oneshot::Receiver<()>>,
}

/// Source of block boundaries.
pub enum Pacer {
    /// Interval-driven slot production.
    Internal(BlockTicker),
    /// Externally supplied block boundaries.
    External(ExternalPacer),
}

/// State for interval-driven slot production.
pub struct BlockTicker {
    /// Next slot to emit.
    slot: Slot,
    /// Block production interval.
    ticker: Interval,
}

impl BlockTicker {
    /// Builds an interval ticker starting at the engine's current slot.
    pub(crate) fn new(engine: &Engine, blocktime: Duration) -> Self {
        let slot = engine.blocks().current_slot();
        let mut ticker = time::interval(blocktime);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticker.reset();
        BlockTicker { slot, ticker }
    }

    /// Returns the next block boundary and advances the slot cursor.
    pub(crate) fn block(&mut self) -> Block {
        let time = unix_time().as_secs() as i64;
        let block = Block::new(self.slot, time);
        self.slot += 1;
        block
    }
}

/// Block boundary submitted by an external producer.
///
/// The caller supplies its slot and timestamp. The sequencer overwrites the
/// hash and parent with locally computed hash-chain metadata.
pub struct ExternalBlock {
    /// Boundary to enqueue.
    pub block: Block,
    /// Notified after the pacemaker handles the boundary locally.
    ///
    /// On ordinary slots this means the boundary was queued and the keeper slot
    /// was advanced. On superblock slots it means the snapshot was taken and
    /// its seal was queued, but the appender may still be sealing it.
    pub submitted: oneshot::Sender<()>,
}

impl ExternalBlock {
    /// Pairs a boundary with the receiver signalled once the pacemaker has locally
    /// handled it, letting the submitter await ordered application.
    pub fn new(block: Block) -> (Self, oneshot::Receiver<()>) {
        let (submitted, guard) = oneshot::channel();
        let block = Self { block, submitted };
        (block, guard)
    }
}

impl PaceMaker {
    /// Registers and starts the pacemaker task.
    ///
    /// Uses an external block source when supplied. Otherwise it records one
    /// reset at the keeper's current slot, clears chain-mirrored volatile state,
    /// and starts emitting slots on the configured block interval.
    pub fn spawn(
        engine: Engine,
        pacer: Option<ExternalPacer>,
        blockstore: BlockstoreParams,
        shutdown: &mut ShutdownManager,
    ) -> Result<()> {
        let pacer = match pacer {
            Some(rx) => Pacer::External(rx),
            None => {
                let ticker = BlockTicker::new(&engine, blockstore.blocktime);
                engine.reset(ticker.slot)?;
                Pacer::Internal(ticker)
            }
        };
        let shutdown = shutdown.handle(Service::PaceMaker);
        let superblock = blockstore.superblock;
        let pacemaker = Self {
            engine,
            pacer,
            superblock,
            sealed: None,
        };
        tokio::spawn(pacemaker.run(shutdown));
        Ok(())
    }

    /// Paces block boundaries until shutdown or the block source is exhausted.
    ///
    /// Shutdown follows the pacing mode. Internal pacing publishes one last
    /// block and flushes durable state. External pacing also checkpoints
    /// volatile state alongside its durable cursor for the next upstream
    /// handshake.
    async fn run(mut self, mut shutdown: ShutdownHandle) {
        let mut res = loop {
            let next = tokio::select! {
                biased;
                _ = shutdown.signalled() => None,
                next = self.next() => next,
            };
            let Some((block, submission)) = next else {
                break Ok(());
            };
            if let Err(error) = self.handle(block).await {
                break Err(error);
            }
            if let Some(submission) = submission {
                let _ = submission.send(());
            }
        };
        res = if let Pacer::Internal(ref mut t) = self.pacer {
            // Await every shutdown step even after an earlier failure.
            let b = t.block();
            res.and(self.handle(b).await).and(self.shutdown(false).await)
        } else {
            res.and(self.shutdown(true).await)
        };
        // Release engine storage before the manager can reopen it.
        drop(self);
        if let Err(error) = res {
            error!(?error, "pace maker terminated with critical failure");
            shutdown.terminate(ShutdownReason::Error(error.into()));
        } else {
            shutdown.terminate(ShutdownReason::Signalled);
        }
    }

    /// Waits for the next block boundary without applying it.
    async fn next(&mut self) -> Option<(Block, Option<oneshot::Sender<()>>)> {
        match &mut self.pacer {
            Pacer::Internal(t) => {
                t.ticker.tick().await;
                Some((t.block(), None))
            }
            Pacer::External(rx) => rx.recv().await.map(|msg| (msg.block, Some(msg.submitted))),
        }
    }

    /// Advances the execution and simulation environments to `block`, sealing a
    /// superblock when the slot lands on the configured interval.
    ///
    /// The snapshot and seal submission run behind a barrier because the
    /// accountsdb export is only coherent while no store operation can race it.
    /// Once the seal is queued, appender FIFO ordering preserves the boundary
    /// while execution resumes and the durable rotation completes in parallel.
    async fn handle(&mut self, block: Block) -> Result<()> {
        self.sequencer.simulation.send(SimulatorMessage::Block(block)).await?;
        if !block.slot.is_multiple_of(self.superblock.get()) {
            self.sequencer.send(SequencerMessage::Block(block)).await?;
            return Ok(());
        }

        let (controller, guard) = runtime::barrier();
        self.sequencer.send(SequencerMessage::Checkpoint(block, guard)).await?;
        controller.acknowledged.await?;
        if let Some(sealed) = self.sealed.take() {
            sealed.await?;
        }
        self.sealed = Some(self.finalize_superblock()?);
        Ok(())
    }
}
