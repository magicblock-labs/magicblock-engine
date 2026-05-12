//! Shared engine configuration types.

use std::{num::NonZeroU64, path::PathBuf, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

/// Local signing identity and optional authority override represented by a replica.
///
/// Serialization includes the complete local keypair as a base58 string.
/// Consumers must redact the `local` field before exposing serialized output.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Authority {
    /// Signer used for locally produced messages and transactions.
    #[serde(with = "keypair")]
    pub local: Arc<Keypair>,
    /// Immediate upstream identity exposed as the engine authority when set.
    #[serde(default, with = "serde_with::As::<Option<serde_with::DisplayFromStr>>")]
    pub remote: Option<Pubkey>,
}

impl Authority {
    /// Returns the remote authority when configured, otherwise the local identity.
    pub fn pubkey(&self) -> Pubkey {
        self.remote.unwrap_or(self.local.pubkey())
    }
}

impl<K: Into<Arc<Keypair>>> From<K> for Authority {
    fn from(local: K) -> Self {
        let local = local.into();
        Self { local, remote: None }
    }
}

/// Account storage and recent-load cache parameters.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct AccountsDBParams {
    /// Accounts database root directory.
    pub directory: PathBuf,
    /// Requested maximum number of resolved account pubkeys retained for
    /// recency tracking and eviction notifications.
    ///
    /// The cache uses at least 256 slots, rounds larger capacities up to a
    /// power of two, and may evict earlier under bucket pressure.
    pub lru_capacity: usize,
}

/// Block production timing used by the engine and keeper caches.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct BlockstoreParams {
    /// Expected wall-clock interval between produced slots.
    pub blocktime: Duration,
    /// Number of blocks included into each superblock.
    pub superblock: NonZeroU64,
}

/// Ledger storage parameters.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LedgerParams {
    /// Ledger root directory.
    pub directory: PathBuf,
    /// Maximum used bytes allowed on the ledger filesystem before eviction runs.
    pub size_limit: u64,
}

mod keypair {
    use super::*;
    use serde::{Deserializer, Serializer, de::Error as _};

    pub(super) fn serialize<S: Serializer>(
        keypair: &Arc<Keypair>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&keypair.to_base58_string())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Arc<Keypair>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        Keypair::try_from_base58_string(&encoded)
            .map(Arc::new)
            .map_err(D::Error::custom)
    }
}
