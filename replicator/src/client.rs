use std::{
    fs::File,
    io::{self, BufReader, Read},
    net::{SocketAddr, TcpStream},
    thread,
};

use derive_more::Deref;
use engine::{Engine, EngineError, pacemaker::ExternalBlock};
use ledger::{
    Superblock,
    schema::{Block, OwnedBlockestoreEntry, blockstore},
};
use nucleus::{
    KB,
    ledger::{ACCOUNTSDB_SNAPSHOT_FILE, BlockstorePosition},
    shutdown::{Service, ShutdownHandle, ShutdownManager, ShutdownReason},
};
use tokio::{
    runtime,
    sync::{broadcast::Receiver, mpsc::Sender},
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

/// Pulls a leader blockstore stream into an externally paced follower engine.
#[derive(Deref)]
pub struct ReplicationClient {
    /// Engine receiving replicated transactions, boundaries, seals, and resets.
    #[deref]
    engine: Engine,
    /// Leader endpoint reused after transport loss.
    addr: SocketAddr,
    /// External pacemaker channel used to preserve block-boundary ordering.
    pacer: Sender<ExternalBlock>,
    /// Locally committed block boundaries used to verify replicated output.
    blocks: Receiver<Block>,
}

impl ReplicationClient {
    /// Starts the follower worker; connection failures are reported through shutdown management.
    pub fn spawn(
        addr: SocketAddr,
        engine: Engine,
        pacer: Sender<ExternalBlock>,
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
            .spawn(move || rt.block_on(client.run(shutdown)))?;
        Ok(())
    }

    /// Consumes the leader stream and reports why the client stopped.
    async fn run(self, mut shutdown: ShutdownHandle) {
        let result = self.consume(&shutdown).await;
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

    /// Reads blockstore entries from the leader, reconnecting on transport loss,
    /// until shutdown is requested or a non-recoverable error occurs.
    async fn consume(mut self, shutdown: &ShutdownHandle) -> Result<()> {
        let mut stream = self.reconnect(shutdown).await?;
        let mut connected = metrics::client_connection();
        loop {
            if shutdown.requested() {
                return Ok(());
            }
            match blockstore::decode(&mut stream) {
                Ok(entry) => self.process(entry).await?,
                Err(wincode::error::ReadError::Io(error)) => {
                    warn!(?error, "replication stream disconnected");
                    drop(connected);
                    stream = self.reconnect(shutdown).await?;
                    connected = metrics::client_connection();
                }
                Err(error) => Err(wincode::Error::from(error))?,
            }
        }
    }

    /// Applies one blockstore entry to the follower engine, holding block-boundary
    /// ordering through the pacemaker and flagging superblock seal mismatches.
    async fn process(&mut self, entry: OwnedBlockestoreEntry) -> Result<()> {
        match entry {
            OwnedBlockestoreEntry::Block(block) => {
                let (external, guard) = ExternalBlock::new(block);
                self.pacer.send(external).await.map_err(EngineError::from)?;
                let pending = time::timeout(IO_TIMEOUT, self.blocks.recv());
                let observed = pending.await?.map_err(|_| ReplicationError::StreamClosed)?;
                if block != observed {
                    // Mismatches are diagnostic until recovery policy is implemented.
                    error!(?block, ?observed, "replication block divergence detected");
                }
                guard.await.map_err(EngineError::from)?;
            }
            OwnedBlockestoreEntry::Superblock(expected) => {
                // The preceding boundary finalized local state; this seal only validates it.
                let observed = self.superblocks().sealed();
                if observed != expected {
                    // Mismatches are diagnostic until recovery policy is implemented.
                    error!(?expected, ?observed, "replication state mismatch detected");
                    metrics::client_state_mismatch();
                }
            }
            entry => self.engine.replay(entry).await?,
        }
        Ok(())
    }

    /// Handshakes with the leader at `position`; either stages a snapshot and
    /// signals a required restart, or returns the resumed byte stream.
    fn connect(&self, position: BlockstorePosition) -> Result<ReplicationStream> {
        let _timer = metrics::time(Operation::ClientConnect);
        let mut connection = TcpStream::connect_timeout(&self.addr, IO_TIMEOUT)?;
        connection.set_read_timeout(Some(IO_TIMEOUT))?;
        connection.set_write_timeout(Some(IO_TIMEOUT))?;
        let request = HandshakeRequest { version: PROTO_VERSION, position };
        let handshake = Handshake::new(self.signer(), request)?;
        protocol::write(&mut connection, &handshake)?;
        let handshake = protocol::read::<Handshake<HandshakeResponse>>(&mut connection)?;
        handshake.verify()?;
        let expected = self.authority();
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
        }
    }

    /// Reconnects from a quiesced local cursor.
    async fn reconnect(&self, shutdown: &ShutdownHandle) -> Result<ReplicationStream> {
        // Hold quiescence so every retry uses the same flushed position.
        let _guard = self.barrier().await?;
        self.sync(false)?;
        let position = self.superblocks().position();
        for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
            if shutdown.requested() {
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
                Err(error) => return Err(error),
            }
            let timeout = RETRY_DELAY * attempt as u32;
            if time::timeout(timeout, shutdown.signalled()).await.is_ok() {
                return Err(ReplicationError::StreamClosed);
            }
        }
        Err(ReplicationError::ReconnectExhausted)
    }

    /// Writes the incoming snapshot archive into a fresh superblock directory and
    /// records its seal, readying the follower to restart from that state.
    fn stage_snapshot(&self, connection: &mut TcpStream, meta: SnapshotMetadata) -> Result<()> {
        let _timer = metrics::time(Operation::ClientStageSnapshot);
        // Stage in the successor before seal rotation so restart can find it.
        let dir = Superblock::init_dir(self.superblocks().directory(), meta.id + 1)?;
        let archive = dir.join(ACCOUNTSDB_SNAPSHOT_FILE);
        let mut file = File::options().write(true).create(true).truncate(true).open(&archive)?;
        let written = io::copy(&mut connection.take(meta.len), &mut file)?;
        file.sync_all()?;
        if written != meta.len {
            return Err(ReplicationError::Snapshot(meta.len, written));
        }
        self.superblocks().append(meta.superblock)?;
        info!(?meta, "replication snapshot staged");
        Ok(())
    }
}
