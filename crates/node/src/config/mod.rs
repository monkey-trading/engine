//! Node configuration: one TOML file, loaded and validated once at startup. Three parts are protocol
//! and go into the log: the client auth (`node.auth`, with `custody.chain_id` and `custody.vault`
//! under `eip712`) and the operator (`operator.public_key`, with the same chain id and vault),
//! each recorded when the log first activates a protocol version that needs it and fixed for the
//! life of the log; and the `[genesis]` section, sequenced after protocol activation. Everything
//! else configures this node only. The SHA-256 of the file is bound into the enclave's
//! attestation, so a verifier can match it against a published copy of the file.

use std::net::{IpAddr, SocketAddr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::VerifyingKey;
use engine_core::Fixed;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::eip712;
use engine_core::market::MarketSpec;
use engine_core::params::ProtocolParams;
use engine_core::staking::StakingConfig;
use engine_core::types::{AccountId, Millis};
use k256::ecdsa;
use serde::de::Error;
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};

mod benchmarks;
mod collateral;
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
mod proxy;
pub use proxy::ProxyProtocolSection;
mod tls;
pub use tls::{AcmeSection, AcmeTestCa, ApiTlsSection};
mod sealing;
pub use sealing::{HandoverSection, SealingSection};

/// Built with the `production` feature: mock TEE, custody, and feed providers are refused.
#[cfg(feature = "production")]
const PRODUCTION_BUILD: bool = true;
#[cfg(not(feature = "production"))]
const PRODUCTION_BUILD: bool = false;

/// 100 ms, the default and the least `api.quote_interval`.
pub(crate) const MIN_QUOTE_INTERVAL: Duration = Duration::from_millis(100);

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

/// Limits on the public API. A client is its socket peer IP, or the address a trusted proxy's PROXY
/// protocol header names, with an IPv6 client counted by its /64; forwarding headers are ignored
/// because the host could forge them.
#[derive(Clone, Debug, Deserialize)]
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
    /// A signed stream connection gets a signed heartbeat at least this often, so its client can
    /// tell a quiet log from a held-back one. At least one second.
    #[serde(default = "default_heartbeat_interval", with = "humantime_serde")]
    pub heartbeat_interval: Duration,
    /// A signed stream connection that asked for an account's quotes gets them again when one
    /// changes, at most this often. At least 100 ms.
    #[serde(default = "default_quote_interval", with = "humantime_serde")]
    pub quote_interval: Duration,
    /// Most markets one quote subscription may name.
    #[serde(default = "default_quote_max_markets")]
    pub quote_max_markets: NonZeroUsize,
    /// Browser origins (`scheme://host[:port]`) that may read the REST API and open the stream. Empty
    /// by default, which sends no CORS headers and refuses a stream upgrade that carries any `Origin`
    /// (a client that sends none still connects). The
    /// operator's `/v1/admin` route is never opened to a browser. A production build takes only
    /// `https` origins.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// A public CA certificate for the API's domain; without it the API serves only its
    /// self-signed certificate, which clients trust by pinning the attested key.
    pub tls: Option<ApiTlsSection>,
    /// A pass-through proxy in front of the node, whose PROXY protocol header names each client.
    /// Absent, the client is the socket peer.
    pub proxy_protocol: Option<ProxyProtocolSection>,
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
    /// The vendor's name for the CPU generation, which keys its certificate chain (for example
    /// Milan or Genoa). Required with an SEV-SNP quote; the node relays that quote's collateral.
    pub processor: Option<String>,
    /// The vendor's key-distribution service the node fetches collateral from.
    #[serde(default = "collateral::default_kds_base")]
    pub kds_base: String,
    /// How often the node refetches the collateral.
    #[serde(default = "collateral::default_refresh", with = "humantime_serde")]
    pub collateral_refresh: Duration,
    /// Seals the enclave's keys on the data disk, so a restart keeps them and the next release can
    /// take them over. A hardware enclave refuses to start without it.
    pub sealing: Option<SealingSection>,
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
    /// Where the node keeps the account history index and the derived marks cache, both rebuilt
    /// from the log: removing the directory while the node is stopped rebuilds them from the log.
    pub dir: PathBuf,
    /// How long an event stays in the index, counted back from the sequenced time of the newest
    /// command it holds. The log keeps every event regardless. After a change, the node discards the
    /// index and rebuilds it from the log.
    #[serde(with = "humantime_serde")]
    pub retention: Duration,
    /// The per-market index-mark history the node serves at `/v1/markets/{id}/marks`; absent runs
    /// the cache with its default retentions and no provider backfill.
    #[serde(default)]
    pub marks: Option<MarksSection>,
}

/// The derived marks cache a node keeps and serves: per-resolution retention for the buckets it
/// prunes, and the optional provider backfill that seeds history a fresh log cannot have.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarksSection {
    /// How long 1s marks stay: the series that fills sub-minute timeframes. 48 hours by default.
    #[serde(default = "default_marks_retention_1s", with = "humantime_serde")]
    pub retention_1s: Duration,
    /// How long 1m marks stay. 60 days by default.
    #[serde(default = "default_marks_retention_1m", with = "humantime_serde")]
    pub retention_1m: Duration,
    /// How long 1h marks stay. 2 years by default.
    #[serde(default = "default_marks_retention_1h", with = "humantime_serde")]
    pub retention_1h: Duration,
    /// How long 1d marks stay; absent keeps them unbounded.
    #[serde(default, with = "humantime_serde::option")]
    pub retention_1d: Option<Duration>,
    /// Fetching historical candles from a provider's public endpoint to seed the cache before a
    /// log has depth of its own.
    pub backfill: Option<BackfillSection>,
}

/// Provider backfill of the marks cache from a public candles endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackfillSection {
    /// Whether to fetch and seed; without it the cache serves log-derived history only.
    pub enabled: bool,
    /// The candles endpoint's base URL. A production build accepts only `https` on the venue's own
    /// domain; other builds also accept an `http` loopback URL for a stand-in server.
    pub base_url: String,
    /// How far back the 1m series is seeded.
    #[serde(with = "humantime_serde")]
    pub depth_1m: Duration,
    /// How far back the 1h series is seeded.
    #[serde(with = "humantime_serde")]
    pub depth_1h: Duration,
    /// How far back the 1d series is seeded.
    #[serde(with = "humantime_serde")]
    pub depth_1d: Duration,
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
    /// The MONKEY staking config, sequenced before the house capital credit; absent starts the
    /// log without staking.
    #[serde(default)]
    pub staking: Option<StakingConfig>,
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
    /// - `[api.tls.acme]` fails validation (see `AcmeSection::validate`)
    /// - `[api.proxy_protocol]` fails validation (see `ApiSection::validate_proxy_protocol`)
    /// - this is a production build and a TEE, custody, or feed provider is `mock`, an allowed
    ///   origin is not `https`, `[api.tls.acme]` is missing, or a trusted proxy range is loopback or
    ///   too wide (see `ApiSection::refuse_unsafe_proxy`)
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: NodeConfig = toml::from_str(&text)
            .map_err(|e| parse_error(&e, &text))
            .with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate().with_context(|| format!("validating {}", path.display()))?;
        cfg.api.refuse_insecure_origins(PRODUCTION_BUILD).with_context(|| format!("validating {}", path.display()))?;
        cfg.refuse_mocks(PRODUCTION_BUILD).with_context(|| format!("validating {}", path.display()))?;
        cfg.api.refuse_self_signed(PRODUCTION_BUILD).with_context(|| format!("validating {}", path.display()))?;
        cfg.api.refuse_unsafe_proxy(PRODUCTION_BUILD).with_context(|| format!("validating {}", path.display()))?;
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

impl MarksSection {
    /// The 1d retention: unbounded when the config sets none.
    /// Error cases:
    /// - it is set and too long to count in milliseconds
    pub fn retention_1d_ms(&self) -> anyhow::Result<Option<Millis>> {
        self.retention_1d.map(|d| Millis::try_from(d.as_millis()).context("too long")).transpose()
    }

    /// Checks the retentions are positive and countable in milliseconds, and the backfill section
    /// where present.
    /// Error cases:
    /// - a retention is zero, or too long to count in milliseconds
    /// - the backfill section fails validation (see `BackfillSection::validate`)
    pub fn validate(&self, production: bool) -> anyhow::Result<()> {
        for (name, retention) in [
            ("retention_1s", self.retention_1s),
            ("retention_1m", self.retention_1m),
            ("retention_1h", self.retention_1h),
        ] {
            ensure!(!retention.is_zero(), "history.marks.{name} must be non-zero");
            Millis::try_from(retention.as_millis()).with_context(|| format!("history.marks.{name}"))?;
        }
        self.retention_1d_ms().context("history.marks.retention_1d")?;
        if let Some(backfill) = &self.backfill {
            backfill.validate(production)?;
        }
        Ok(())
    }
}

impl BackfillSection {
    /// Checks the depths are positive and countable in milliseconds, and the endpoint is the
    /// venue's own over TLS, or loopback HTTP outside a production build for a stand-in server.
    /// Error cases:
    /// - a depth is zero, or too long to count in milliseconds
    /// - `base_url` does not parse, is not `https` on the venue's domain, or is plaintext outside
    ///   loopback or in a production build
    pub fn validate(&self, production: bool) -> anyhow::Result<()> {
        for (name, depth) in [("depth_1m", self.depth_1m), ("depth_1h", self.depth_1h), ("depth_1d", self.depth_1d)] {
            ensure!(!depth.is_zero(), "history.marks.backfill.{name} must be non-zero");
            Millis::try_from(depth.as_millis()).with_context(|| format!("history.marks.backfill.{name}"))?;
        }
        let url = reqwest::Url::parse(&self.base_url).context("history.marks.backfill.base_url")?;
        let host = url.host_str().context("history.marks.backfill.base_url has no host")?;
        match url.scheme() {
            "https" => ensure!(
                host == "coinbase.com" || host.ends_with(".coinbase.com"),
                "history.marks.backfill.base_url must be on the venue's own domain"
            ),
            "http" => {
                let loopback = url.host_str().is_some_and(|host| {
                    host == "localhost"
                        || host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
                });
                ensure!(
                    loopback && !production,
                    "history.marks.backfill.base_url allows plaintext HTTP only on loopback, and only outside a production build"
                );
            }
            other => bail!("history.marks.backfill.base_url scheme {other:?} is not http or https"),
        }
        Ok(())
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

/// Five seconds, the default `api.heartbeat_interval`.
fn default_heartbeat_interval() -> Duration {
    Duration::from_secs(5)
}

/// Two days of 1s marks: the finest series the cache keeps, for sub-minute timeframes.
pub(crate) fn default_marks_retention_1s() -> Duration {
    Duration::from_secs(48 * 60 * 60)
}

/// Sixty days of 1m marks.
pub(crate) fn default_marks_retention_1m() -> Duration {
    Duration::from_secs(60 * 24 * 60 * 60)
}

/// Two years of 1h marks.
pub(crate) fn default_marks_retention_1h() -> Duration {
    Duration::from_secs(2 * 365 * 24 * 60 * 60)
}

fn default_quote_interval() -> Duration {
    MIN_QUOTE_INTERVAL
}

/// Four, the default `api.quote_max_markets`.
fn default_quote_max_markets() -> NonZeroUsize {
    NonZeroUsize::new(4).expect("four is not zero")
}

/// Decodes a base64 Ed25519 public key.
/// Error cases:
/// - the text is not base64, does not decode to 32 bytes, or is not a valid Ed25519 key
fn public_key<'de, D: Deserializer<'de>>(deserializer: D) -> Result<VerifyingKey, D::Error> {
    let bytes = STANDARD.decode(String::deserialize(deserializer)?).map_err(D::Error::custom)?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| D::Error::custom("an Ed25519 public key is 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes).map_err(D::Error::custom)
}

mod origins;
mod test_keys;
pub(crate) use test_keys::is_published_operator;
mod validation;

#[cfg(test)]
mod api_tests;
#[cfg(test)]
mod custody_tests;
#[cfg(test)]
mod feeds_tests;
#[cfg(test)]
mod marks_tests;
#[cfg(test)]
mod origins_tests;
#[cfg(test)]
mod production_tests;
#[cfg(test)]
mod proxy_tests;
#[cfg(test)]
mod telemetry_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tls_tests;

#[cfg(test)]
mod mock_key_tests;
