use std::{
    fs::{self, File},
    io::{self, BufReader, Read},
    mem,
    net::{SocketAddr, TcpStream},
    thread::{self, JoinHandle},
};

use derive_more::Deref;
use engine::{
    Engine, EngineError, TransactionAccessor, TransactionVerifier, VerifiedTransaction,
    pacemaker::ExternalBlock,
};
use flume::{Receiver, Sender, TrySendError};
use ledger::{
    Superblock,
    schema::{Block, OwnedBlockstoreEntry, Reset, Signed, SuperblockSeal, blockstore},
};
use nucleus::{
    KB,
    ledger::{ACCOUNTSDB_SNAPSHOT_FILE, BlockstorePosition},
    runtime::{BarrierHandle, BlockInput},
    shutdown::{Service, ShutdownHandle, ShutdownManager, ShutdownReason},
};
use solana_pubkey::Pubkey;
use tokio::{runtime, sync::mpsc::Sender as PacerSender, time};
use tracing::{info, warn};

use crate::{
    IO_TIMEOUT, MAX_RECONNECT_ATTEMPTS, RETRY_DELAY, ReplicationError, Result,
    metrics::{self, Operation},
    protocol::{
        self, Handshake, HandshakeRequest, HandshakeResponse, PROTO_VERSION, SnapshotMetadata,
    },
};

type ReplicationStream = BufReader<TcpStream>;

/// Maximum transactions retained before offering a batch to Control.
const MAX_BATCH_TRANSACTIONS: usize = 128;
/// Maximum transaction payload bytes retained before offering a batch to Control.
const MAX_BATCH_BYTES: usize = 128 * KB;

/// Consecutive transaction payloads accumulated between ordered stream fences.
#[derive(Default)]
struct TransactionsBatch {
    transactions: Vec<Vec<u8>>,
    bytes: usize,
}

impl TransactionsBatch {
    fn push(&mut self, transaction: Vec<u8>) {
        self.bytes = self.bytes.saturating_add(transaction.len());
        self.transactions.push(transaction);
    }

    fn is_full(&self) -> bool {
        self.transactions.len() >= MAX_BATCH_TRANSACTIONS || self.bytes >= MAX_BATCH_BYTES
    }
}

/// Ordered handoff from blocking Ingest to asynchronous Control.
enum ReplicationMessage {
    /// Raw transactions awaiting authority and signature verification.
    Unverified(Vec<Vec<u8>>),
    /// Transactions verified by Ingest while Control was occupied.
    Verified(Vec<VerifiedTransaction>),
    /// Block boundary fenced behind every preceding batch.
    Block(Signed<Block>),
    /// Superblock seal fenced behind every preceding batch.
    Superblock(Signed<SuperblockSeal>),
    /// Volatile-state reset fenced behind every preceding batch.
    Reset(Signed<Reset>),
}

/// Why connection-scoped Ingest stopped without a terminal replication error.
enum IngestExit {
    /// Control dropped its receiver after reaching a boundary or terminal error.
    Stopped,
    /// The transport failed after all preceding entries were handed to Control.
    Disconnected(wincode::io::ReadError),
}

/// Why Control stopped consuming one connection.
enum ControlExit {
    /// Normal shutdown reached and flushed a validated block boundary.
    Boundary(BlockstorePosition),
    /// Ingest ended; its join result determines whether to reconnect or fail.
    HandoffClosed,
}

/// Decodes one connection and opportunistically verifies bounded transaction batches.
struct Ingest {
    /// Blocking stream for one authenticated connection.
    stream: ReplicationStream,
    /// Transactions accumulated until a size or entry fence.
    batch: TransactionsBatch,
    /// Rendezvous handoff preserving decoded stream order.
    tx: Sender<ReplicationMessage>,
    /// Authority-bound verifier used when Control is occupied.
    verifier: TransactionVerifier,
    /// Configured upstream signer for boundary and reset records.
    authority: Pubkey,
}

/// Pulls a leader blockstore stream into an externally paced follower engine.
#[derive(Deref)]
pub struct ReplicationClient {
    /// Engine receiving replicated state.
    #[deref]
    engine: Engine,
    /// Leader endpoint reused for reconnects.
    addr: SocketAddr,
    /// External block source for the follower pacemaker.
    pacer: PacerSender<ExternalBlock>,
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
        let client = Self { engine, addr, pacer };
        let rt = runtime::Builder::new_current_thread().enable_time().build()?;
        thread::Builder::new()
            .name("replication-client".into())
            .spawn(move || rt.block_on(client.serve(shutdown)))?;
        Ok(())
    }

    /// Reports the terminal client outcome to shutdown management.
    async fn serve(self, mut shutdown: ShutdownHandle) {
        let reason = match self.run(&shutdown).await {
            Ok(position) => {
                info!(?position, "replication stopped at a durable boundary");
                ShutdownReason::Signalled
            }
            Err(ReplicationError::RestartRequired(slot)) => {
                info!(%slot, "replication client has requested node restart");
                ShutdownReason::RestartRequired
            }
            Err(error) => ShutdownReason::Error(Box::new(error)),
        };
        shutdown.terminate(reason);
    }

    /// Owns connection recovery and returns success only at a validated block boundary.
    async fn run(mut self, shutdown: &ShutdownHandle) -> Result<BlockstorePosition> {
        let (mut barrier, mut position) = self.resume().await?;
        loop {
            let stream = self.reconnect(position).await?;
            let connected = metrics::client_connection();
            drop(barrier);

            let (rx, ingest) = Ingest::spawn(stream, self.verifier(), self.authority())?;
            let control = self.consume(shutdown, &rx).await;
            drop(rx);
            let ingest = ingest.join().map_err(|_| ReplicationError::IngestPanicked)?;
            drop(connected);

            match control {
                Ok(ControlExit::Boundary(position)) => return Ok(position),
                Err(error) => return Err(error),
                Ok(ControlExit::HandoffClosed) => match ingest? {
                    IngestExit::Disconnected(error) => {
                        warn!(%error, "replication stream disconnected");
                        (barrier, position) = self.resume().await?;
                    }
                    IngestExit::Stopped => return Err(ReplicationError::IngestStopped),
                },
            }
        }
    }

    /// Consumes one Ingest stream, draining normal shutdown to the next block.
    async fn consume(
        &mut self,
        shutdown: &ShutdownHandle,
        rx: &Receiver<ReplicationMessage>,
    ) -> Result<ControlExit> {
        let verifier = self.verifier();
        let mut draining = shutdown.requested();
        loop {
            let message = tokio::select! {
                biased;
                message = rx.recv_async() => match message {
                    Ok(message) => message,
                    Err(_) => return Ok(ControlExit::HandoffClosed),
                },
                _ = shutdown.signalled(), if !draining => {
                    draining = true;
                    continue;
                },
            };
            match message {
                ReplicationMessage::Unverified(batch) => {
                    self.schedule(verifier.verify(batch)?).await?;
                }
                ReplicationMessage::Verified(batch) => self.schedule(batch).await?,
                ReplicationMessage::Block(block) => {
                    let (external, guard) = ExternalBlock::new(BlockInput::Replay(block));
                    self.pacer.send(external).await.map_err(EngineError::from)?;
                    time::timeout(IO_TIMEOUT, guard).await?.map_err(EngineError::from)?;
                    if draining {
                        let (_guard, position) = self.resume().await?;
                        return Ok(ControlExit::Boundary(position));
                    }
                }
                ReplicationMessage::Superblock(expected) => {
                    // The original seal arrives after its block. Quiesce before
                    // validating/exporting, and durably rotate before consuming more.
                    let _guard = self.engine.barrier().await?;
                    let sealed = self.engine.finalize_superblock(Some(expected))?;
                    sealed.await.map_err(EngineError::from)?;
                }
                ReplicationMessage::Reset(reset) => {
                    let _guard = self.engine.barrier().await?;
                    self.engine.append_reset(reset)?;
                }
            }
        }
    }

    /// Schedules a verified batch in stream order.
    async fn schedule(&self, transactions: Vec<VerifiedTransaction>) -> Result<()> {
        for transaction in transactions {
            TransactionAccessor::verified(&self.engine, transaction).schedule().await?;
        }
        Ok(())
    }

    async fn resume(&self) -> Result<(BarrierHandle, BlockstorePosition)> {
        let guard = self.barrier().await?;
        self.superblocks().sync(false)?;
        Ok((guard, self.superblocks().position()))
    }

    /// Retries transport establishment from one quiesced durable cursor.
    async fn reconnect(&self, position: BlockstorePosition) -> Result<ReplicationStream> {
        for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
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
            let delay = RETRY_DELAY * attempt as u32;
            time::interval_at(time::Instant::now() + delay, delay).tick().await;
        }
        Err(ReplicationError::ReconnectExhausted)
    }

    /// Authenticates one resume request and returns its blockstore stream.
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
                Err(ReplicationError::RestartRequired(
                    meta.superblock.payload.id,
                ))
            }
            HandshakeResponse::Stream(remote) => {
                info!(?position, ?remote, "replication handshake accepted");
                Ok(BufReader::with_capacity(256 * KB, connection))
            }
            HandshakeResponse::Err(message) => Err(ReplicationError::Handshake(message)),
            HandshakeResponse::StreamActive => Err(ReplicationError::StreamActive),
        }
    }

    /// Durably stages a snapshot and records its bootstrap seal.
    fn stage_snapshot(&self, connection: &mut TcpStream, meta: SnapshotMetadata) -> Result<()> {
        let _timer = metrics::time(Operation::ClientStageSnapshot);
        if !meta.superblock.verify(&self.authority()) {
            return Err(ReplicationError::InvalidSignature("superblock"));
        }
        let superblocks = self.engine.superblocks();
        let dir = Superblock::init_dir(superblocks.directory(), meta.superblock.payload.id + 1)?;
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
        superblocks.append(meta.superblock)?.recv().map_err(EngineError::from)?;
        info!(?meta, "replication snapshot staged");
        Ok(())
    }
}

impl Ingest {
    /// Starts one blocking decoder for an authenticated connection.
    fn spawn(
        stream: ReplicationStream,
        verifier: TransactionVerifier,
        authority: Pubkey,
    ) -> Result<(Receiver<ReplicationMessage>, JoinHandle<Result<IngestExit>>)> {
        let (tx, rx) = flume::bounded(0);
        let ingest = Self {
            stream,
            batch: Default::default(),
            tx,
            verifier,
            authority,
        };
        let worker = thread::Builder::new()
            .name("replication-ingest".into())
            .spawn(move || ingest.run())?;
        Ok((rx, worker))
    }

    /// Decodes until transport loss, terminal failure, or Control exit.
    fn run(mut self) -> Result<IngestExit> {
        loop {
            let entry = match blockstore::decode(&mut self.stream) {
                Ok(entry) => entry,
                Err(wincode::error::ReadError::Io(error)) => {
                    if !self.flush()? {
                        return Ok(IngestExit::Stopped);
                    }
                    return Ok(IngestExit::Disconnected(error));
                }
                Err(error) => {
                    if !self.flush()? {
                        return Ok(IngestExit::Stopped);
                    }
                    return Err(wincode::Error::from(error).into());
                }
            };
            let message = match entry {
                OwnedBlockstoreEntry::Transaction(transaction) => {
                    self.batch.push(transaction);
                    if self.batch.is_full() && !self.flush()? {
                        return Ok(IngestExit::Stopped);
                    }
                    continue;
                }
                OwnedBlockstoreEntry::Block(block) => {
                    if !block.verify(&self.authority) {
                        return Err(ReplicationError::InvalidSignature("block"));
                    }
                    ReplicationMessage::Block(block)
                }
                OwnedBlockstoreEntry::Superblock(seal) => {
                    if !seal.verify(&self.authority) {
                        return Err(ReplicationError::InvalidSignature("superblock"));
                    }
                    ReplicationMessage::Superblock(seal)
                }
                OwnedBlockstoreEntry::Reset(reset) => {
                    if !reset.verify(&self.authority) {
                        return Err(ReplicationError::InvalidSignature("reset"));
                    }
                    ReplicationMessage::Reset(reset)
                }
            };
            if !self.flush()? || self.tx.send(message).is_err() {
                return Ok(IngestExit::Stopped);
            }
        }
    }

    /// Offers raw work to idle Control, otherwise verifies without losing stream order.
    fn flush(&mut self) -> Result<bool> {
        if self.batch.transactions.is_empty() {
            return Ok(true);
        }
        let batch = mem::take(&mut self.batch).transactions;
        match self.tx.try_send(ReplicationMessage::Unverified(batch)) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(ReplicationMessage::Unverified(batch))) => {
                let verified = self.verifier.verify(batch)?;
                Ok(self.tx.send(ReplicationMessage::Verified(verified)).is_ok())
            }
            Err(_) => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, net::TcpListener, time::Duration};

    use engine::testkit::TestEngine;
    use solana_keypair::Keypair;

    use super::*;

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
    /// payloads and wrong-key signatures for blocks, seals, and resets.
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
            // Change payloads after signing, independently for each scenario.
            block.payload.slot += tamper;
            seal.payload.checksum += tamper;
            reset.payload.0 += tamper;
            for (kind, record) in [
                ("block", OwnedBlockstoreEntry::Block(block)),
                ("superblock", OwnedBlockstoreEntry::Superblock(seal)),
                ("reset", OwnedBlockstoreEntry::Reset(reset)),
            ] {
                let case = format!("{kind}, {scenario}");
                let bytes = wincode::serialize(&record).unwrap();
                let (message, result) = ingest(&engine, &bytes, &case);
                if scenario == "valid" {
                    let accepted = match message {
                        Some(ReplicationMessage::Block(block)) => {
                            OwnedBlockstoreEntry::Block(block)
                        }
                        Some(ReplicationMessage::Superblock(seal)) => {
                            OwnedBlockstoreEntry::Superblock(seal)
                        }
                        Some(ReplicationMessage::Reset(reset)) => {
                            OwnedBlockstoreEntry::Reset(reset)
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
}
