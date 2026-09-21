//! Block-boundary pacing.

use std::time::Duration;

use derive_more::Deref;
use ledger::schema::Block;
use nucleus::{
    Slot,
    config::BlockstoreParams,
    runtime::{self, BlockInput},
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
pub(crate) struct PaceMaker {
    /// Engine handle used to submit each boundary.
    #[deref]
    engine: Engine,
    /// Source for the next block boundary.
    pacer: Pacer,
    /// Number of slots sealed into each superblock.
    superblock: u64,
    /// Independent checksum interval; zero disables checkpoint production.
    checkpoint: u64,
    /// Completion of the last queued seal, awaited before taking its successor.
    sealed: Option<oneshot::Receiver<()>>,
}

/// Source of block boundaries.
enum Pacer {
    /// Interval-driven slot production.
    Internal(BlockTicker),
    /// Externally supplied block boundaries.
    External(ExternalPacer),
}

/// State for interval-driven slot production.
struct BlockTicker {
    /// Next slot to emit.
    slot: Slot,
    /// Block production interval.
    ticker: Interval,
}

impl BlockTicker {
    /// Builds an interval ticker starting at the engine's current slot.
    fn new(engine: &Engine, blocktime: Duration) -> Self {
        let slot = engine.blocks().current_slot();
        let mut ticker = time::interval(blocktime);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticker.reset();
        BlockTicker { slot, ticker }
    }

    /// Returns the next block boundary and advances the slot cursor.
    fn block(&mut self) -> Block {
        let time = unix_time().as_secs() as i64;
        let block = Block::new(self.slot, time);
        self.slot += 1;
        block
    }
}

/// Externally supplied production or authenticated replay boundary.
pub struct ExternalBlock {
    /// Boundary to enqueue.
    pub block: BlockInput,
    /// Notified by the sequencer after validation and application finish.
    pub submitted: oneshot::Sender<()>,
}

impl ExternalBlock {
    /// Pairs a boundary with its sequencer completion acknowledgment.
    pub fn new(block: BlockInput) -> (Self, oneshot::Receiver<()>) {
        let (submitted, guard) = oneshot::channel();
        (Self { block, submitted }, guard)
    }
}

impl PaceMaker {
    /// Registers and starts the pacemaker task.
    ///
    /// Uses an external block source when supplied. Otherwise it records one
    /// reset at the keeper's current slot, clears chain-mirrored volatile state,
    /// and starts emitting slots on the configured block interval.
    pub(crate) fn spawn(
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
            checkpoint: blockstore.checkpoint,
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
            if let Err(error) = self.handle(block, submission).await {
                break Err(error);
            }
        };
        let external = matches!(self.pacer, Pacer::External(_));
        if let Pacer::Internal(ticker) = &mut self.pacer {
            let block = ticker.block();
            res = res.and(self.handle(BlockInput::Production(block), None).await);
        }
        // Flush storage even if pacing or the final block failed.
        res = res.and(self.shutdown(external).await);
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
    async fn next(&mut self) -> Option<(BlockInput, Option<oneshot::Sender<()>>)> {
        match &mut self.pacer {
            Pacer::Internal(t) => {
                t.ticker.tick().await;
                Some((BlockInput::Production(t.block()), None))
            }
            Pacer::External(rx) => rx.recv().await.map(|msg| (msg.block, Some(msg.submitted))),
        }
    }

    /// Advances to `block`, sealing a superblock or sampling a checksum when due.
    ///
    /// The snapshot and seal submission run behind a barrier because the
    /// accountsdb export is only coherent while no store operation can race it.
    /// Once the seal is queued, appender FIFO ordering preserves the boundary
    /// while execution resumes and the durable rotation completes in parallel.
    async fn handle(&mut self, input: BlockInput, tx: Option<oneshot::Sender<()>>) -> Result<()> {
        let block = input.payload();
        self.sequencer.simulation.send(SimulatorMessage::Block(block)).await?;
        let seal = block.slot.is_multiple_of(self.superblock);
        let checkpoint = self.checkpoint != 0 && block.slot.is_multiple_of(self.checkpoint);
        if matches!(input, BlockInput::Replay(_)) || !(seal || checkpoint) {
            self.sequencer.send(SequencerMessage::Block { block: input, tx }).await?;
            return Ok(());
        }

        let (controller, guard) = runtime::barrier();
        let msg = SequencerMessage::Checkpoint { block, tx, guard };
        self.sequencer.send(msg).await?;
        controller.acknowledged.await?;
        if !seal {
            // SAFETY: the checkpoint barrier holds execution until this scope ends.
            unsafe { self.checkpoint(None) }?;
            return Ok(());
        }
        if let Some(sealed) = self.sealed.take() {
            sealed.await?;
        }
        self.sealed = Some(self.finalize_superblock(None)?);
        Ok(())
    }
}
