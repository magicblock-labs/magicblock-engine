use std::{
    fs::{self, File},
    io::{self, BufReader, Read},
    net::{SocketAddr, TcpStream},
    sync::mpsc,
    thread,
};

use derive_more::Deref;
use engine::{
    Engine, EngineError, ReplayError, TransactionAccessor, VerifiedTransaction,
    pacemaker::ExternalBlock,
};
use flume::{Sender, TrySendError};
use ledger::{
    Superblock,
    schema::{Block, OwnedBlockstoreEntry, blockstore},
};
use nucleus::{
    KB,
    ledger::{ACCOUNTSDB_SNAPSHOT_FILE, BlockstorePosition},
    runtime::BarrierHandle,
    shutdown::{CancellationToken, Service, ShutdownHandle, ShutdownManager, ShutdownReason},
};
use tokio::{
    runtime,
    sync::mpsc::{Receiver as BlockReceiver, Sender as PacerSender},
    time,
};
use tracing::{error, info, warn};

use crate::{
    IO_TIMEOUT, MAX_RECONNECT_ATTEMPTS, RETRY_DELAY, ReplicationError, Result,
    metrics::{self, Operation},
    protocol::{
        self, Handshake, HandshakeRequest, HandshakeResponse, PROTO_VERSION, SnapshotMetadata,
    },
};

type ReplicationStream = BufReader<TcpStream>;
type ReconnectReply = mpsc::SyncSender<BlockstorePosition>;

/// Maximum transactions retained before offering a batch to Control.
const MAX_BATCH_TRANSACTIONS: usize = 128;
/// Maximum transaction payload bytes retained before offering a batch to Control.
const MAX_BATCH_BYTES: usize = 128 * KB;

/// Consecutive transaction payloads accumulated between ordered stream fences.
struct TransactionsBatch {
    /// Raw transaction payloads in stream order.
    transactions: Vec<Vec<u8>>,
    /// Cumulative payload bytes used to enforce the batch bound.
    bytes: usize,
}

impl Default for TransactionsBatch {
    fn default() -> Self {
        Self {
            transactions: Vec::with_capacity(MAX_BATCH_TRANSACTIONS),
            bytes: 0,
        }
    }
}

impl TransactionsBatch {
    fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }

    fn push(&mut self, transaction: Vec<u8>) {
        self.bytes = self.bytes.saturating_add(transaction.len());
        self.transactions.push(transaction);
    }

    fn is_full(&self) -> bool {
        self.transactions.len() >= MAX_BATCH_TRANSACTIONS || self.bytes >= MAX_BATCH_BYTES
    }

    fn take(&mut self) -> Vec<Vec<u8>> {
        self.bytes = 0;
        std::mem::replace(
            &mut self.transactions,
            Vec::with_capacity(MAX_BATCH_TRANSACTIONS),
        )
    }
}

/// Ordered handoff from blocking stream ingest to asynchronous Engine control.
enum ReplicationMessage {
    /// Raw batch offered to Control for signature verification.
    Unverified(Vec<Vec<u8>>),
    /// Verification result completed by Ingest while Control was occupied.
    Verified(engine::Result<Vec<VerifiedTransaction>>),
    /// Control entry fenced behind every preceding transaction batch.
    Entry(OwnedBlockstoreEntry),
    /// Successful handshake allowing Control to release its sequencing barrier.
    Connected,
    /// Lost stream requesting Control's next durable resume position.
    Disconnected(ReconnectReply),
}

/// Owns stream decoding, bounded transaction accumulation, and opportunistic verification.
struct Ingest {
    /// Engine used for batch verification and authenticated stream recovery.
    engine: Engine,
    /// Upstream replication endpoint reused across reconnects.
    addr: SocketAddr,
    /// Consecutive transactions awaiting an ordered handoff.
    batch: TransactionsBatch,
    /// Rendezvous sender preserving ingest-to-Control message order.
    tx: Sender<ReplicationMessage>,
    /// Cancellation scoped to the ingest worker lifecycle.
    shutdown: CancellationToken,
}

/// Pulls a leader blockstore stream into an externally paced follower engine.
#[derive(Deref)]
pub struct ReplicationClient {
    /// Engine receiving replicated transactions, boundaries, seals, and resets.
    #[deref]
    engine: Engine,
    /// Leader endpoint reused after transport loss.
    addr: SocketAddr,
    /// External pacemaker channel used to preserve block-boundary ordering.
    pacer: PacerSender<ExternalBlock>,
    /// Locally committed block boundaries used to verify replicated output.
    blocks: BlockReceiver<Block>,
}

impl ReplicationClient {
    /// Starts the follower worker; connection failures are reported through shutdown management.
    pub fn spawn(
        addr: SocketAddr,
        engine: Engine,
        pacer: PacerSender<ExternalBlock>,
        shutdown: &mut ShutdownManager,
    ) -> Result<()> {
        metrics::init();
        let shutdown = shutdown.handle(Service::ReplicationClient);
        let mut blocks = engine.blocks().subscribe();
        // drain the channel from potential leftovers
        while blocks.try_recv().is_ok() {}
        let client = Self { engine, addr, pacer, blocks };
        let rt = runtime::Builder::new_current_thread().enable_time().build()?;
        thread::Builder::new()
            .name("replication-client".into())
            .spawn(move || rt.block_on(client.serve(shutdown)))?;
        Ok(())
    }

    /// Consumes the leader stream and reports why the client stopped.
    async fn serve(self, mut shutdown: ShutdownHandle) {
        let result = self.run(&shutdown).await;
        if shutdown.requested() || result.is_ok() {
            shutdown.terminate(ShutdownReason::Signalled);
            return;
        }
        match result {
            Err(ReplicationError::RestartRequired(slot)) => {
                info!(%slot, "replication client has requested node restart");
                shutdown.terminate(ShutdownReason::RestartRequired);
            }
            Err(error) => {
                shutdown.terminate(ShutdownReason::Error(Box::new(error)));
            }
            Ok(()) => (),
        }
    }

    /// Starts Ingest and joins it after Control stops consuming its ordered messages.
    async fn run(self, shutdown: &ShutdownHandle) -> Result<()> {
        let (guard, position) = self.resume().await?;
        let (tx, rx) = flume::bounded(0);
        let cancellation = shutdown.child();
        let mut ingest = Ingest {
            engine: self.engine.clone(),
            addr: self.addr,
            batch: Default::default(),
            tx,
            shutdown: cancellation.clone(),
        };
        let rt = runtime::Builder::new_current_thread().enable_time().build()?;
        let ingest = thread::Builder::new()
            .name("replication-ingest".into())
            .spawn(move || rt.block_on(ingest.run(position)))?;
        let mut result = self.consume(shutdown, rx, guard).await;
        cancellation.cancel();
        match ingest.join() {
            Ok(Ok(())) => info!("replication ingest has gracefully shutdown"),
            Ok(Err(error)) => result = result.and(Err(error)),
            Err(error) => error!(?error, "replication ingest panicked"),
        }
        result
    }

    /// Consumes ordered Ingest messages until shutdown or a terminal failure.
    async fn consume(
        mut self,
        shutdown: &ShutdownHandle,
        rx: flume::Receiver<ReplicationMessage>,
        guard: BarrierHandle,
    ) -> Result<()> {
        let verifier = self.verifier();
        let mut connected = None;
        let mut barrier = Some(guard);

        loop {
            if shutdown.requested() {
                return Ok(());
            }
            // Complete a ready handoff before observing concurrent cancellation.
            let message = tokio::select! {
                biased;
                message = rx.recv_async() => match message {
                    Ok(m) => m,
                    // Ingest has shutdown, the potential error will be captured by caller
                    Err(_) => return Ok(()),
                },
                _ = shutdown.signalled() => break,

            };
            match message {
                ReplicationMessage::Unverified(batch) => {
                    let verified = verifier.verify(batch)?;
                    self.schedule(verified).await?;
                }
                ReplicationMessage::Verified(result) => self.schedule(result?).await?,
                ReplicationMessage::Entry(entry) => self.process(entry).await?,
                ReplicationMessage::Connected => {
                    connected = Some(metrics::client_connection());
                    barrier.take();
                }
                ReplicationMessage::Disconnected(reply) => {
                    connected.take();
                    let (guard, position) = self.resume().await?;
                    barrier = Some(guard);
                    if reply.send(position).is_err() {
                        return Err(ReplicationError::StreamClosed);
                    }
                }
            }
        }
        Ok(())
    }

    /// Schedules a verified batch in stream order without repeating admission checks.
    async fn schedule(&self, transactions: Vec<VerifiedTransaction>) -> Result<()> {
        for transaction in transactions {
            TransactionAccessor::verified(&self.engine, transaction).schedule().await?;
        }
        Ok(())
    }

    /// Applies one control entry after all preceding transactions are scheduled.
    async fn process(&mut self, entry: OwnedBlockstoreEntry) -> Result<()> {
        match entry {
            OwnedBlockstoreEntry::Block(block) => {
                let (external, guard) = ExternalBlock::new(block);
                self.pacer.send(external).await.map_err(EngineError::from)?;
                let pending = time::timeout(IO_TIMEOUT, self.blocks.recv());
                let observed = pending.await?.ok_or(ReplicationError::StreamClosed)?;
                if block != observed {
                    let error = ReplayError::BlockhashMismatch(block.slot);
                    Err(EngineError::from(error))?;
                }
                guard.await.map_err(EngineError::from)?;
            }
            OwnedBlockstoreEntry::Superblock(expected) => {
                // The preceding boundary finalized local state; this seal only validates it.
                let observed = self.superblocks().sealed();
                if observed != expected {
                    error!(?expected, ?observed, "replication state mismatch detected");
                    metrics::client_state_mismatch();
                    Err(EngineError::Replay(ReplayError::StateMismatch))?;
                }
            }
            OwnedBlockstoreEntry::Reset(slot) => {
                self.engine.replay(OwnedBlockstoreEntry::Reset(slot)).await?;
            }
            OwnedBlockstoreEntry::Transaction(_) => (),
        }
        Ok(())
    }

    /// Flushes prior work and returns its durable cursor under a sequencing barrier.
    async fn resume(&self) -> Result<(BarrierHandle, BlockstorePosition)> {
        let guard = self.barrier().await?;
        self.sync(false)?;
        Ok((guard, self.superblocks().position()))
    }
}

impl Ingest {
    /// Decodes the stream while preserving the order of transactions and control entries.
    async fn run(&mut self, position: BlockstorePosition) -> Result<()> {
        let mut stream = self.open(position).await?;
        while !self.shutdown.is_cancelled() {
            match blockstore::decode(&mut stream) {
                Ok(OwnedBlockstoreEntry::Transaction(transaction)) => {
                    if !self.push(transaction) {
                        return Ok(());
                    }
                }
                Ok(entry) => {
                    if !self.flush() {
                        return Ok(());
                    }
                    self.tx
                        .send(ReplicationMessage::Entry(entry))
                        .map_err(|_| ReplicationError::StreamClosed)?;
                }
                Err(wincode::error::ReadError::Io(error)) => {
                    warn!(%error, "replication stream disconnected");
                    drop(stream);
                    if !self.flush() {
                        return Ok(());
                    }
                    let position = self.request_position()?;
                    stream = self.open(position).await?;
                }
                Err(error) => {
                    if !self.flush() {
                        return Ok(());
                    }
                    return Err(wincode::Error::from(error).into());
                }
            }
        }
        Ok(())
    }

    /// Adds a transaction and flushes once either batch bound is reached.
    fn push(&mut self, transaction: Vec<u8>) -> bool {
        self.batch.push(transaction);
        !self.batch.is_full() || self.flush()
    }

    /// Offers the batch to Control, verifying it locally when Control is occupied.
    fn flush(&mut self) -> bool {
        if self.batch.is_empty() {
            return true;
        }
        let batch = self.batch.take();
        match self.tx.try_send(ReplicationMessage::Unverified(batch)) {
            Ok(()) => true,
            Err(TrySendError::Full(ReplicationMessage::Unverified(batch))) => {
                let result = self.engine.verifier().verify(batch);
                let valid = result.is_ok();
                self.tx.send(ReplicationMessage::Verified(result)).is_ok() && valid
            }
            Err(_) => false,
        }
    }

    /// Requests a durable resume cursor after Control finishes all preceding work.
    fn request_position(&self) -> Result<BlockstorePosition> {
        let (reply, response) = mpsc::sync_channel(0);
        let _ = self.tx.send(ReplicationMessage::Disconnected(reply));
        response.recv().map_err(|_| ReplicationError::StreamClosed)
    }

    /// Reconnects from `position` and tells Control it may release the barrier.
    async fn open(&mut self, position: BlockstorePosition) -> Result<ReplicationStream> {
        let stream = self.reconnect(position).await?;
        let _ = self.tx.send(ReplicationMessage::Connected);
        Ok(stream)
    }

    /// Retries transport establishment while the ordered resume cursor remains quiesced.
    async fn reconnect(&self, position: BlockstorePosition) -> Result<ReplicationStream> {
        for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
            if self.shutdown.is_cancelled() {
                return Err(ReplicationError::StreamClosed);
            }
            metrics::client_connection_attempt();
            match self.connect(position) {
                Ok(stream) => {
                    info!(attempt, ?position, "replication stream connected");
                    return Ok(stream);
                }
                Err(ReplicationError::IO(error)) => {
                    warn!(attempt, ?error, "replication reconnect failed");
                }
                Err(ReplicationError::StreamActive) => {
                    warn!(attempt, "previous replication stream is still active");
                }
                Err(error) => return Err(error),
            }
            let timeout = RETRY_DELAY * attempt as u32;
            if time::timeout(timeout, self.shutdown.cancelled()).await.is_ok() {
                Err(ReplicationError::StreamClosed)?;
            }
        }
        Err(ReplicationError::ReconnectExhausted)
    }

    /// Handshakes at `position`, staging a snapshot when streaming cannot resume.
    fn connect(&self, position: BlockstorePosition) -> Result<ReplicationStream> {
        let _timer = metrics::time(Operation::ClientConnect);
        let mut connection = TcpStream::connect_timeout(&self.addr, IO_TIMEOUT)?;
        connection.set_read_timeout(Some(IO_TIMEOUT))?;
        connection.set_write_timeout(Some(IO_TIMEOUT))?;
        let request = HandshakeRequest { version: PROTO_VERSION, position };
        let handshake = Handshake::new(self.engine.signer(), request)?;
        protocol::write(&mut connection, &handshake)?;
        let handshake = protocol::read::<Handshake<HandshakeResponse>>(&mut connection)?;
        handshake.verify()?;
        let expected = self.engine.authority();
        if handshake.identity != expected {
            let message = format!(
                "unexpected replication server identity {}; expected {expected}",
                handshake.identity
            );
            return Err(ReplicationError::Handshake(message));
        }

        match handshake.payload {
            HandshakeResponse::Snapshot(meta) => {
                self.stage_snapshot(&mut connection, meta)?;
                Err(ReplicationError::RestartRequired(meta.id))
            }
            HandshakeResponse::Stream(remote) => {
                info!(?position, ?remote, "replication handshake accepted");
                Ok(BufReader::with_capacity(256 * KB, connection))
            }
            HandshakeResponse::Err(message) => Err(ReplicationError::Handshake(message)),
            HandshakeResponse::StreamActive => Err(ReplicationError::StreamActive),
        }
    }

    /// Stages a complete snapshot and installs its seal for the requested restart.
    fn stage_snapshot(&self, connection: &mut TcpStream, meta: SnapshotMetadata) -> Result<()> {
        let _timer = metrics::time(Operation::ClientStageSnapshot);
        // Stage in the successor before seal rotation so restart can find it.
        let superblocks = self.engine.superblocks();
        let dir = Superblock::init_dir(superblocks.directory(), meta.id + 1)?;
        let archive = dir.join(ACCOUNTSDB_SNAPSHOT_FILE);
        let temporary = dir.join(format!("{ACCOUNTSDB_SNAPSHOT_FILE}.tmp"));
        let mut file = File::options().write(true).create(true).truncate(true).open(&temporary)?;
        let written = io::copy(&mut connection.take(meta.len), &mut file)?;
        if written != meta.len {
            return Err(ReplicationError::Snapshot(meta.len, written));
        }
        file.sync_all()?;
        drop(file);
        fs::rename(temporary, archive)?;
        superblocks.bootstrap(meta.superblock)?;
        info!(?meta, "replication snapshot staged");
        Ok(())
    }
}
