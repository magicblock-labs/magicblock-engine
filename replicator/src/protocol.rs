use std::{
    io::{Read, Write},
    time::Duration,
};

use derive_more::Deref;
use ledger::schema::SuperblockSeal;
use nucleus::{ledger::BlockstorePosition, unix_time};
use solana_keypair::{Keypair, Signer};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use wincode::{SchemaRead, SchemaWrite, config::DefaultConfig};

use crate::{ReplicationError, Result};

/// Wire protocol version accepted by this crate.
pub const PROTO_VERSION: u32 = 1;

/// Encoded length prefix preceding every control frame.
const HEADER_LENGTH: usize = size_of::<u32>();
/// Largest control frame accepted before allocating its payload.
const MAX_CONTROL_FRAME_LENGTH: u32 = u16::MAX as u32;
/// Maximum clock difference accepted for a signed control message.
const MAX_CLOCK_SKEW: Duration = Duration::from_secs(30);

/// Signed, freshness-bounded control message exchanged during negotiation.
#[derive(SchemaRead, SchemaWrite)]
pub(crate) struct Handshake<P> {
    /// Direction-specific handshake payload.
    pub(crate) payload: P,
    /// Public identity of the signer.
    pub(crate) identity: Pubkey,
    /// Unix timestamp in microseconds covered by the signature.
    pub(crate) timestamp: u64,
    /// Signature over the payload and timestamp.
    pub(crate) signature: Signature,
}

/// Initial follower request identifying its last durable blockstore byte.
#[derive(SchemaRead, SchemaWrite)]
pub(crate) struct HandshakeRequest {
    /// Wire version understood by the follower.
    pub(crate) version: u32,
    /// Next durable blockstore byte required by the follower.
    pub(crate) position: BlockstorePosition,
}

/// Leader decision following a valid handshake.
#[derive(SchemaRead, SchemaWrite)]
pub(crate) enum HandshakeResponse {
    /// Snapshot that must be staged before replication can resume.
    Snapshot(SnapshotMetadata),
    /// Leader cursor from which live streaming begins.
    Stream(BlockstorePosition),
    /// Reason the leader rejected negotiation.
    Err(String),
    /// The follower identity still owns an earlier stream.
    StreamActive,
}

/// Describes the accountsdb snapshot a follower must stage before it can stream.
#[derive(SchemaRead, SchemaWrite, Debug, Clone, Copy, Deref)]
pub(crate) struct SnapshotMetadata {
    /// Length of the snapshot archive in bytes.
    pub(crate) len: u64,
    /// Seal the snapshot restores accountsdb to.
    #[deref]
    pub(crate) superblock: SuperblockSeal,
}

impl<P> Handshake<P>
where
    for<'de> P: SchemaRead<'de, DefaultConfig, Dst = P>,
    P: SchemaWrite<DefaultConfig, Src = P>,
{
    /// Signs the payload with a fresh timestamp and publishes the signer's identity.
    pub(crate) fn new(keypair: &Keypair, payload: P) -> Result<Self> {
        let identity = keypair.pubkey();
        let timestamp = timestamp();
        let data = message(timestamp, &payload)?;
        let signature = keypair.sign_message(&data);
        Ok(Self {
            payload,
            identity,
            timestamp,
            signature,
        })
    }

    /// Rejects altered messages and timestamps outside the accepted clock-skew window.
    ///
    /// Freshness is time-based; duplicate messages inside the window are not tracked.
    pub(crate) fn verify(&self) -> Result<()> {
        let data = message(self.timestamp, &self.payload)?;
        if !self.signature.verify(self.identity.as_ref(), &data) {
            let msg = "invalid handshake signature";
            return Err(ReplicationError::Handshake(msg.into()));
        }
        let skew = timestamp().abs_diff(self.timestamp);
        if skew > MAX_CLOCK_SKEW.as_micros() as u64 {
            let msg = "handshake timestamp exceeds maximum clock skew";
            return Err(ReplicationError::Handshake(msg.into()));
        }
        Ok(())
    }
}

/// Reads and decodes one length-prefixed control message.
pub(crate) fn read<T>(reader: &mut impl Read) -> Result<T>
where
    for<'de> T: SchemaRead<'de, DefaultConfig, Dst = T>,
{
    let mut header = [0; HEADER_LENGTH];
    reader.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header);
    if len > MAX_CONTROL_FRAME_LENGTH {
        return Err(ReplicationError::Handshake(format!(
            "replication control frame len {len} exceeds max allowed"
        )));
    }

    let mut payload = vec![0; len as usize];
    reader.read_exact(&mut payload)?;
    wincode::deserialize_exact(&payload)
        .map_err(wincode::Error::from)
        .map_err(Into::into)
}

/// Encodes and writes one length-prefixed control message.
pub(crate) fn write<T>(writer: &mut impl Write, message: &T) -> Result<()>
where
    T: SchemaWrite<DefaultConfig, Src = T> + ?Sized,
{
    let payload = wincode::serialize(message).map_err(wincode::Error::from)?;
    let len = payload.len() as u32;
    if len > MAX_CONTROL_FRAME_LENGTH {
        return Err(ReplicationError::Handshake(format!(
            "replication control frame len {len} exceeds max allowed",
        )));
    }
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush().map_err(Into::into)
}

fn timestamp() -> u64 {
    unix_time().as_micros() as u64
}

/// Builds the protocol byte string covered by a handshake signature.
fn message<P>(ts: u64, payload: &P) -> Result<Vec<u8>>
where
    P: SchemaWrite<DefaultConfig, Src = P>,
{
    let mut data = wincode::serialize(payload).map_err(wincode::Error::from)?;
    data.extend_from_slice(&ts.to_le_bytes());
    Ok(data)
}
