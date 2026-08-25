use std::{
    fs::File,
    io::Write,
    net::{SocketAddr, TcpStream},
    sync::Arc,
    thread,
};

use derive_more::Deref;
use engine::Engine;
use ledger::schema::SuperblockSeal;
use nucleus::{
    ledger::{ACCOUNTSDB_SNAPSHOT_FILE, BlockstorePosition},
    shutdown::{CancellationToken, Service, ShutdownHandle, ShutdownManager, ShutdownReason},
};
use scc::HashMap;
use solana_keypair::Signer;
use solana_pubkey::Pubkey;
use tokio::{
    net::{TcpListener, TcpStream as AsyncTcpStream},
    runtime,
    sync::broadcast,
};
use tracing::{error, info, warn};

use crate::{
    IO_TIMEOUT, ReplicationError, Result,
    metrics::{self, Operation},
    protocol::{
        self, Handshake, HandshakeRequest, HandshakeResponse, PROTO_VERSION, SnapshotMetadata,
    },
};

/// Accepts follower connections and assigns each one a blocking transfer worker.
pub struct ReplicationDispatcher {
    /// Accepts inbound follower connections.
    listener: TcpListener,
    /// Engine whose local signer authenticates responses and whose ledger is served.
    engine: Engine,
    /// List of follower identities permitted to replicate.
    allowed: Arc<HashMap<Pubkey, Arc<()>>>,
    /// Cancels the accept loop and parents every per-connection worker.
    shutdown: ShutdownHandle,
}

/// Serves one follower from its requested durable cursor onward.
#[derive(Deref)]
struct ReplicationServer {
    /// Blocking, timeout-bounded socket to the follower.
    connection: TcpStream,
    /// Cursor of the next byte owed to the follower.
    position: ReplicationPosition,
    /// Engine whose local signer authenticates responses and whose ledger is served.
    #[deref]
    engine: Engine,
    /// Local follower identities permitted to replicate.
    allowed: Arc<HashMap<Pubkey, Arc<()>>>,
    /// Fires when the dispatcher shuts down.
    cancellation: CancellationToken,
}

/// Open blockstore and cursor from which the next byte must be sent.
struct ReplicationPosition {
    /// Open blockstore file for `current.superblock`.
    blockstore: File,
    /// Durable-cursor updates broadcast by the appender.
    stream: broadcast::Receiver<BlockstorePosition>,
    /// Position of the next byte to send.
    current: BlockstorePosition,
}

/// Initial transfer selected after validating the follower cursor.
enum ReplicationAction {
    /// Send a full accountsdb snapshot; the follower restarts from it.
    Snapshot { archive: File, meta: SnapshotMetadata },
    /// Resume the blockstore stream from a still-retained cursor.
    Stream { from: BlockstorePosition, blockstore: File },
}

impl ReplicationDispatcher {
    /// Verifies the canonical signer, then binds `addr` and starts the accept loop.
    pub async fn spawn(
        addr: SocketAddr,
        engine: Engine,
        allowed: Arc<[Pubkey]>,
        shutdown: &mut ShutdownManager,
    ) -> Result<()> {
        metrics::init();
        if engine.signer().pubkey() != engine.authority() {
            warn!("dispatcher is disabled: node cannot act as replication relay");
            return Ok(());
        }
        let listener = TcpListener::bind(addr).await?;
        let shutdown = shutdown.handle(Service::ReplicationDispatcher);
        let allowed = Arc::new(allowed.iter().map(|&identity| (identity, Arc::new(()))).collect());
        let service = Self {
            listener,
            engine,
            allowed,
            shutdown,
        };
        tokio::spawn(service.run());
        info!(%addr, "replication dispatcher started");
        Ok(())
    }

    /// Accepts until cancellation or a listener failure; connection failures stay isolated.
    async fn run(mut self) {
        let reason = loop {
            tokio::select! {
                biased;
                _ = self.shutdown.signalled() => break ShutdownReason::Signalled,
                result = self.listener.accept() => match result {
                    Ok((stream, peer)) => {
                        if let Err(error) = self.dispatch(stream, peer) {
                            warn!(%peer, ?error, "failed to dispatch replication connection");
                        }
                    }
                    Err(error) => break ShutdownReason::Error(Box::new(error)),
                }
            }
        };
        // Release owned resources before reporting service termination.
        drop(self.listener);
        drop(self.engine);
        self.shutdown.terminate(reason);
    }

    /// Converts the accepted async socket into a blocking, timeout-bounded stream
    /// and hands it to a dedicated blocking server worker.
    fn dispatch(&self, stream: AsyncTcpStream, peer: SocketAddr) -> Result<()> {
        let stream = stream.into_std()?;
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let engine = self.engine.clone();
        let cancellation = self.shutdown.child();
        let allowed = self.allowed.clone();
        ReplicationServer::spawn(stream, peer, engine, allowed, cancellation)
    }
}

impl ReplicationServer {
    /// Starts a blocking worker without blocking the async dispatcher.
    fn spawn(
        connection: TcpStream,
        peer: SocketAddr,
        engine: Engine,
        allowed: Arc<HashMap<Pubkey, Arc<()>>>,
        cancellation: CancellationToken,
    ) -> Result<()> {
        // Subscribe before sampling so racing cursor updates remain queued.
        let stream = engine.ledger().position.subscribe();
        let current = engine.ledger().position();
        let blockstore = blockstore(&engine, current.superblock)?;
        let position = ReplicationPosition { stream, current, blockstore };
        let server = Self {
            connection,
            position,
            engine,
            allowed,
            cancellation,
        };
        let runtime = runtime::Builder::new_current_thread().build()?;
        thread::Builder::new().name("replication-server".into()).spawn(move || {
            let _connection = metrics::server_connection();
            runtime
                .block_on(server.run())
                .inspect_err(|error| warn!(%peer, %error, "replication connection failed"))
        })?;
        Ok(())
    }

    /// Negotiates an initial transfer, catches up immediately, then follows durable cursors.
    async fn run(mut self) -> Result<()> {
        // Cursor updates arrive at every block and write new durable bytes. Those writes
        // detect peer disconnects, exit this worker, and release its identity lease.
        let (action, _lease) = match self.handshake() {
            Ok(handshake) => handshake,
            Err(error) => {
                warn!(?error, "replication handshake rejected");
                let response = match error {
                    ReplicationError::StreamActive => HandshakeResponse::StreamActive,
                    other => HandshakeResponse::Err(other.to_string()),
                };
                self.respond(response)?;
                return Ok(());
            }
        };

        match action {
            ReplicationAction::Snapshot { mut archive, meta } => {
                let _timer = metrics::time(Operation::ServerSendSnapshot);
                info!(?meta, "sending replication snapshot");
                let response = HandshakeResponse::Snapshot(meta);
                self.respond(response)?;
                send_range(&mut archive, &mut self.connection, 0, meta.len)?;
                self.connection.flush()?;
                return Ok(());
            }
            ReplicationAction::Stream { from, blockstore } => {
                let response = HandshakeResponse::Stream(self.position.current);
                self.respond(response)?;
                self.position.current = from;
                self.position.blockstore = blockstore;
                let through = self.ledger().position();
                self.advance(through)?;
                info!(?from, ?through, "replication caught up");
            }
        }

        loop {
            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => return Ok(()),
                result = self.position.stream.recv() => match result {
                    Ok(position) => self.advance(position)?,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        let position = self.ledger().position();
                        // Cursors are cumulative; advancing to latest covers skipped updates.
                        warn!(skipped, ?position, "replication cursor lagged");
                        metrics::server_cursor_updates_skipped(skipped);
                        self.advance(position)?;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        error!("ledger position stream closed unexpectedly");
                        return Err(ReplicationError::CursorStreamClosed);
                    }
                }
            }
        }
    }

    /// Signs and writes a leader handshake response with the local engine signer.
    fn respond(&mut self, response: HandshakeResponse) -> Result<()> {
        let handshake = Handshake::new(self.signer(), response)?;
        protocol::write(&mut self.connection, &handshake)
    }

    /// Selects retained streaming when possible, otherwise the newest ready snapshot.
    fn handshake(&mut self) -> Result<(ReplicationAction, Arc<()>)> {
        let _timer = metrics::time(Operation::ServerHandshake);
        let handshake: Handshake<HandshakeRequest> = protocol::read(&mut self.connection)?;
        handshake.verify()?;
        if handshake.payload.version != PROTO_VERSION {
            return Err(ReplicationError::VersionMismatch(PROTO_VERSION));
        }
        let lease = self.reserve(handshake.identity)?;

        let requested = handshake.payload.position;
        if requested > self.position.current {
            return Err(ReplicationError::PositionNotFound(requested));
        }
        let action = match self.ledger().cursor(requested.superblock) {
            Some(end) if requested.offset <= end => ReplicationAction::Stream {
                from: requested,
                blockstore: blockstore(self, requested.superblock)?,
            },
            Some(_) => return Err(ReplicationError::PositionNotFound(requested)),
            None => self.snapshot()?,
        };
        Ok((action, lease))
    }

    /// Falls back to the newest retained superblock that has a staged accountsdb
    /// snapshot, used when the follower's cursor is no longer streamable.
    fn snapshot(&self) -> Result<ReplicationAction> {
        for superblock in self.ledger().iter() {
            let path = superblock.directory.join(ACCOUNTSDB_SNAPSHOT_FILE);
            if !path.exists() {
                continue;
            }
            let archive = File::open(path)?;
            let meta = SnapshotMetadata {
                len: archive.metadata()?.len(),
                // Successor archives carry the predecessor seal metadata.
                superblock: SuperblockSeal {
                    id: superblock.id.saturating_sub(1),
                    checksum: superblock.checksum(),
                    transactions: superblock.transactions(),
                },
            };
            return Ok(ReplicationAction::Snapshot { archive, meta });
        }
        Err(ReplicationError::SnapshotUnavailable)
    }

    /// Sends every durable byte between the current cursor and `target`.
    fn advance(&mut self, target: BlockstorePosition) -> Result<()> {
        let _timer = metrics::time(Operation::ServerAdvance);
        if target <= self.position.current {
            return Ok(());
        }
        while self.position.current.superblock < target.superblock {
            let end = self
                .ledger()
                .cursor(self.position.current.superblock)
                .ok_or(ReplicationError::PositionNotFound(self.position.current))?;
            self.send(end)?;
            let superblock = self.position.current.superblock + 1;
            self.position.blockstore = blockstore(self, superblock)?;
            self.position.current = BlockstorePosition { superblock, offset: 0 };
        }
        self.send(target.offset)
    }

    /// Sends blockstore bytes from the current offset up to `end`, advancing the cursor.
    fn send(&mut self, end: u64) -> Result<()> {
        let start = self.position.current.offset;
        send_range(
            &mut self.position.blockstore,
            &mut self.connection,
            start,
            end - start,
        )?;
        self.position.current.offset = end;
        Ok(())
    }

    /// Reserves an allowed identity until the returned lease drops.
    fn reserve(&self, identity: Pubkey) -> Result<Arc<()>> {
        let Some(entry) = self.allowed.get_sync(&identity) else {
            let msg = "replication access not allowed";
            return Err(ReplicationError::Handshake(msg.into()));
        };
        if Arc::strong_count(entry.get()) > 1 {
            return Err(ReplicationError::StreamActive);
        }
        Ok(entry.get().clone())
    }
}

/// Copies `len` bytes from `file` at `offset` to the socket, looping until the
/// full range is transferred (each `send_exact` may transfer only part of it).
fn send_range(
    file: &mut File,
    stream: &mut TcpStream,
    mut offset: u64,
    mut len: u64,
) -> Result<()> {
    while len != 0 {
        let sent = snedfile::send_exact(file, stream, len, offset)?;
        offset += sent;
        len -= sent;
    }
    Ok(())
}

/// Clones the blockstore file handle of a retained superblock for independent seeking.
fn blockstore(engine: &Engine, superblock: u64) -> Result<File> {
    let position = BlockstorePosition { superblock, offset: 0 };
    let candidate = engine.ledger().iter().find(|sb| sb.id == superblock);
    let sb = candidate.ok_or(ReplicationError::PositionNotFound(position))?;
    sb.blockstore.try_clone().map_err(Into::into)
}
