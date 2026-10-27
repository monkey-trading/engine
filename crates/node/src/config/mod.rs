//! Node configuration: one TOML file, loaded and validated once at startup. Three parts are protocol
//! and go into the log: the client auth (`node.auth`, with `custody.chain_id` and `custody.vault`
//! under `eip712`) and the operator (`operator.public_key`, with the same chain id and vault),
//! each recorded when the log first activates a protocol version that needs it and fixed for the
//! life of the log; and the `[genesis]` section, sequenced after protocol activation. Everything
//! else configures this node only. The SHA-256 of the file is bound into the enclave's
//! attestation, so a verifier can match it against a published copy of the file.

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::VerifyingKey;
use engine_core::Fixed;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::eip712;
use engine_core::market::MarketSpec;
use engine_core::params::ProtocolParams;
use engine_core::types::{AccountId, Millis};
use k256::ecdsa;
use serde::de::Error;
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};

mod benchmarks;
pub use benchmarks::{BenchmarkSection, BenchmarkSource, Publisher};
mod access;
pub use access::AccessSection;
mod custody;
pub use custody::{EvmCustodySection, FinalityTag, RpcProviderSection, SecretUrl};
mod feeds;
pub use feeds::*;
mod checkpoints;
pub use checkpoints::CheckpointSection;
mod telemetry;
pub use crate::config::telemetry::*;

/// Built with the `production` feature: mock TEE, custody, and feed providers are refused.
#[cfg(feature = "production")]
const PRODUCTION_BUILD: bool = true;
#[cfg(not(feature = "production"))]
const PRODUCTION_BUILD: bool = false;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Trusts the `account` field on requests. Local development only: it needs the mock TEE and
    /// mock custody.
    None,
    /// Requests must carry an EIP-712 signature from the account's key.
    Eip712,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeeProvider {
    /// No hardware; attestation reports are clearly marked as fake.
    Mock,
    /// Linux configfs-tsm (`/sys/kernel/config/tsm/report`), which serves both Intel TDX and AMD
    /// SEV-SNP confidential VMs.
    ConfigfsTsm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustodyProvider {
    /// No chain. Exposes a dev deposit endpoint and logs withdrawals.
    Mock,
    /// Credits canonical vault deposits from an EVM chain. Withdrawals are refused until they are
    /// paid on-chain.
    Evm,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub node: NodeSection,
    pub api: ApiSection,
    pub logging: LoggingSection,
    pub tee: TeeSection,
    pub operator: OperatorSection,
    pub custody: CustodySection,
    pub checkpoints: Option<CheckpointSection>,
    pub feeds: FeedsSection,
    pub time: TimeSection,
    pub history: HistorySection,
    pub snapshots: Option<SnapshotSection>,
    pub metrics: MetricsSection,
    pub alerts: AlertsSection,
    /// Enables startup synchronization with an external signed publisher worker.
    pub benchmarks: Option<BenchmarkSection>,
    pub access: Option<AccessSection>,
    /// Sequenced as admin commands after protocol activation on a log's first boot, all or nothing,
    /// so the rules every fill ran under are part of the public record.
    pub genesis: Genesis,
    /// SHA-256 of the file `load` read, which the attestation binds; not a config key.
    #[serde(skip)]
    pub file_sha256: [u8; 32],
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSection {
    pub listen: SocketAddr,
    /// The log's segment files and `manifest.jsonl`. Completed segments and the manifest are the
    /// public archive, served at `/v1/archive/`; every segment is kept, since recovery replays the
    /// log from genesis.
    pub log_dir: PathBuf,
    /// Start a new log segment once the current one holds this many commands.
    pub segment_commands: u64,
    /// Write a signed state root every N commands.
    pub checkpoint_every: u64,
    /// Interval of `Tick` commands that drive sessions and rollovers.
    #[serde(with = "humantime_serde")]
    pub tick: Duration,
    pub auth: AuthMode,
}

/// Limits on the public API. A client is its socket peer IP, with an IPv6 client counted by its
/// /64; forwarding headers are ignored because the host could forge them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiSection {
    /// Open connections, in total and from one client; one over either is closed unread. Keep the
    /// total below the process's open-file limit, or accepts fail before the cap is reached.
    pub max_connections: NonZeroUsize,
    pub max_connections_per_ip: NonZeroUsize,
    /// A connection that has not finished its TLS handshake by then is closed.
    #[serde(with = "humantime_serde")]
    pub handshake_timeout: Duration,
    /// A client gets this long to send a request's headers, and again its body; a connection idle
    /// this long between requests is closed.
    #[serde(with = "humantime_serde")]
    pub read_timeout: Duration,
    /// Largest request body, which gets 413 when larger, and largest message a stream subscriber
    /// may send, which ends its stream when larger.
    pub max_body_bytes: NonZeroUsize,
    /// Requests from one client, and requests that act for one account; excess requests get 429.
    pub per_ip: RateLimit,
    pub per_account: RateLimit,
    /// New connections from one client; one over it is closed before its TLS handshake. Set it
    /// above `per_ip`, so a client that opens a connection per request gets 429 first.
    pub per_ip_connections: RateLimit,
    /// Open WebSocket stream subscriptions; one more gets 503.
    pub max_subscribers: NonZeroUsize,
    /// A client that takes longer than this to take a response, or a stream subscriber one
    /// message, is disconnected.
    #[serde(with = "humantime_serde")]
    pub send_timeout: Duration,
}

/// A token bucket: `burst` requests at once, refilled at `per_second`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    pub per_second: NonZeroU32,
    pub burst: NonZeroU32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingSection {
    /// A `tracing` filter directive, e.g. `"info"` or `"info,engine_node=debug"`.
    pub level: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeeSection {
    pub provider: TeeProvider,
    /// Uses the public development signer only with simulated attestation.
    #[serde(default)]
    pub use_published_mock_key: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorSection {
    /// Operator rotation is off at launch: one operator runs the enclave while the product
    /// settles. When it's switched on, operators hand over by passing the log and the sealed
    /// signing key between attested enclaves.
    pub rotation_enabled: bool,
    /// The operator's secp256k1 public key, SEC1-encoded hex, which signs every admin command
    /// `engine admin` sends. The node holds no operator secret.
    pub public_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustodySection {
    pub provider: CustodyProvider,
    pub chain_id: u64,
    /// The vault contract that holds collateral.
    pub vault: String,
    /// The chain connection; present exactly when `provider = "evm"`.
    pub evm: Option<EvmCustodySection>,
}

/// Trusted time: the Roughtime servers the host clock is checked against.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeSection {
    /// The most a sequenced timestamp may differ from Roughtime-verified time. A reply slower than
    /// this cannot place the clock within it, so it is also how long each query waits.
    #[serde(with = "humantime_serde")]
    pub max_drift: Duration,
    /// How often to query the servers.
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// Servers run by independent operators; a sample needs replies from a strict majority.
    ///
    /// This file sits on host storage; the attestation binds its SHA-256, so a verifier checks these
    /// keys against a published copy of the config.
    pub servers: Vec<RoughtimeServer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoughtimeServer {
    pub name: String,
    /// `host:port`, queried over UDP.
    pub address: String,
    /// The server's long-term Ed25519 public key, in base64 as its operator publishes it.
    #[serde(deserialize_with = "public_key")]
    pub public_key: VerifyingKey,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistorySection {
    /// Where the node keeps the account history index, which it derives from the log alone:
    /// removing the directory while the node is stopped rebuilds the index from the log.
    pub dir: PathBuf,
    /// How long an event stays in the index, counted back from the sequenced time of the newest
    /// command it holds. The log keeps every event regardless. After a change, the node discards the
    /// index and rebuilds it from the log.
    #[serde(with = "humantime_serde")]
    pub retention: Duration,
}

/// Optional signed recovery caches; signer pins must come from verified attestation.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotSection {
    pub dir: PathBuf,
    pub retain: NonZeroUsize,
    pub max_bytes: NonZeroUsize,
    pub trusted_signers: Vec<AccountId>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Genesis {
    /// House capital credited at genesis. Zero credits none; with EVM custody it must be zero,
    /// since house capital then enters only through vault deposits to the house.
    pub house_capital: Fixed,
    pub params: ProtocolParams,
    #[serde(default)]
    pub benchmarks: Vec<Benchmark>,
    pub markets: Vec<MarketSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Benchmark {
    pub name: String,
    pub annual_bps: i64,
}

impl NodeConfig {
    /// Reads and validates the config file at `path`, and records the SHA-256 of the bytes it read.
    /// Error cases:
    /// - the file cannot be read, or is not valid TOML for this schema (unknown keys included);
    ///   the error names the line but never quotes it, since a line may hold a credential
    /// - `logging.level` is not a valid filter directive, or one that disables the node's errors
    /// - `node.tick`, `node.checkpoint_every`, `node.segment_commands`, `feeds.interval`,
    ///   `time.max_drift`, `time.interval`, or an `api` timeout is zero, or `time.interval` plus
    ///   twice `time.max_drift` is not under 60s
    /// - `node.auth` is `none` with a TEE or custody provider other than `mock`
    /// - `tee.provider` is not `mock` and `operator.public_key` is a published test key, listed in
    ///   `config/test-keys.toml`
    /// - `[alerts.critical]` is missing with a TEE or custody provider other than `mock`
    /// - `feeds.venues` is missing for `provider = "venues"`, fails validation, or has a `timeout`
    ///   no longer than `feeds.interval`
    /// - `time.servers` has fewer than two servers, lists a public key twice, or has an address
    ///   that is not `host:port`
    /// - `history.retention` is zero, or too long to count in milliseconds
    /// - the custody section does not match its provider (see `validate_custody`)
    /// - `operator.public_key` or `custody.vault` does not parse
    /// - genesis lists a market id, a market symbol, or a benchmark name twice
    /// - the engine would reject a genesis command
    /// - this is a production build and a TEE, custody, or feed provider is `mock`
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: NodeConfig = toml::from_str(&text)
            .map_err(|e| parse_error(&e, &text))
            .with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate().with_context(|| format!("validating {}", path.display()))?;
        cfg.refuse_mocks(PRODUCTION_BUILD).with_context(|| format!("validating {}", path.display()))?;
        cfg.file_sha256 = Sha256::digest(text.as_bytes()).into();
        Ok(cfg)
    }

    /// The client auth this config asks the log to record: unsigned, or signed under the domain of
    /// `custody.vault` on `custody.chain_id`.
    /// Error cases:
    /// - `custody.vault` is not an address
    pub fn client_auth(&self) -> anyhow::Result<ClientAuth> {
        Ok(match self.node.auth {
            AuthMode::None => ClientAuth::Unsigned,
            AuthMode::Eip712 => {
                let vault: AccountId = self.custody.vault.parse().context("custody.vault")?;
                ClientAuth::Eip712 { chain_id: self.custody.chain_id, vault }
            }
        })
    }

    /// The operator this config asks the log to record: the address of `operator.public_key`, signing
    /// under the domain of `custody.vault` on `custody.chain_id`.
    /// Error cases:
    /// - `operator.public_key` is not a SEC1-encoded secp256k1 public key in hex
    /// - `custody.vault` is not an address
    pub fn operator(&self) -> anyhow::Result<Operator> {
        let address = public_key_address(&self.operator.public_key).context("operator.public_key")?;
        let vault: AccountId = self.custody.vault.parse().context("custody.vault")?;
        Ok(Operator { address, chain_id: self.custody.chain_id, vault })
    }
}

impl HistorySection {
    /// The retention in milliseconds, the unit of sequenced time.
    /// Error cases:
    /// - the retention is zero, which would empty the index at every write
    /// - it is too long to count in milliseconds
    pub fn retention_ms(&self) -> anyhow::Result<Millis> {
        ensure!(!self.retention.is_zero(), "must be non-zero");
        Millis::try_from(self.retention.as_millis()).context("too long")
    }
}

/// A TOML error reduced to its message and line. The error's own text quotes the offending
/// source line, which may hold a credential such as `custody.evm.providers[].rpc_url`.
fn parse_error(e: &toml::de::Error, text: &str) -> anyhow::Error {
    let line = e.span().map(|span| text[..span.start].matches('\n').count() + 1);
    match line {
        Some(line) => anyhow::anyhow!("line {line}: {}", e.message()),
        None => anyhow::anyhow!("{}", e.message()),
    }
}

/// The account address of `key`, a SEC1-encoded secp256k1 public key in hex, compressed or not.
/// Error cases:
/// - `key` is not one
fn public_key_address(key: &str) -> anyhow::Result<AccountId> {
    let key = hex::decode(key.trim_start_matches("0x"))
        .ok()
        .and_then(|bytes| ecdsa::VerifyingKey::from_sec1_bytes(&bytes).ok())
        .context("not a SEC1-encoded secp256k1 public key in hex")?;
    Ok(eip712::address(&key))
}

/// Decodes a base64 Ed25519 public key.
/// Error cases:
/// - the text is not base64, does not decode to 32 bytes, or is not a valid Ed25519 key
fn public_key<'de, D: Deserializer<'de>>(deserializer: D) -> Result<VerifyingKey, D::Error> {
    let bytes = STANDARD.decode(String::deserialize(deserializer)?).map_err(D::Error::custom)?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| D::Error::custom("an Ed25519 public key is 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes).map_err(D::Error::custom)
}

mod test_keys;
mod validation;

#[cfg(test)]
mod custody_tests;
#[cfg(test)]
mod feeds_tests;
#[cfg(test)]
mod production_tests;
#[cfg(test)]
mod telemetry_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod mock_key_tests;
