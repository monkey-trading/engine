//! The watcher's config: one TOML file, loaded and validated once at startup. It holds no key of
//! the node's, only what to verify (the node and its release manifest), what to follow (the logs),
//! and where alerts go (the same `[alerts]` shape as the node's).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use reqwest::Url;
use serde::Deserialize;

use crate::config::{LoggingSection, MetricsSection, OutboxSection, WebhookRoute};
use crate::ct_watch::log_client::LogKey;
use crate::verify::KDS_BASE_URL;

/// The longest hostname DNS allows.
const MAX_DOMAIN_LEN: usize = 253;
/// Bounds a log's name, which labels its alerts and its state.
const MAX_LOG_NAME_LEN: usize = 64;

/// The watcher's whole config file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchConfig {
    /// The API's domain, lowercase: certificates for it (or a wildcard covering it) are checked.
    pub domain: String,
    /// The watcher's durable state: attested key hashes, log positions, and open findings.
    pub state_file: PathBuf,
    /// How often the node's attestation is fetched and verified. At most half of `grace`, so a
    /// restart's new key is seen before the wait on its certificate ends.
    #[serde(with = "humantime_serde")]
    pub attestation_interval: Duration,
    /// How often each log is polled for new entries.
    #[serde(with = "humantime_serde")]
    pub log_interval: Duration,
    /// How long a certificate for an unknown key waits for an attestation that names the key
    /// before it raises an alert: a restart can get its certificate before the watcher polls.
    #[serde(with = "humantime_serde", default = "default_grace")]
    pub grace: Duration,
    /// A log not read up to a signed tree head no older than this, for this long, raises
    /// `ct_log_unreachable`.
    #[serde(with = "humantime_serde", default = "default_log_stale_after")]
    pub log_stale_after: Duration,
    pub node: WatchedNode,
    pub logs: Vec<LogSection>,
    pub logging: LoggingSection,
    /// The operator listener for `GET /metrics`: the alert delivery counters the node's Prometheus
    /// rules watch, and whether the watcher answers at all. Keep it off any public interface.
    pub metrics: MetricsSection,
    pub alerts: WatchAlerts,
}

/// The node whose attestation the watcher verifies.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchedNode {
    /// The node's API, `https://host:port`. The attestation is verified, not the connection, so a
    /// self-signed or CA-issued certificate both serve; plain HTTP to loopback is for dev only.
    pub url: String,
    /// The release manifest the attestation is verified against.
    pub release: PathBuf,
    /// The vendor processor name for the certificate chain (for example Milan or Genoa).
    pub processor: String,
    /// The vendor key-distribution service's base URL.
    #[serde(default = "default_kds")]
    pub kds: String,
    /// An extra root certificate, ASCII PEM, pinning the quote's chain in addition to the vendor
    /// roots; test and private chains verify against it.
    pub root: Option<PathBuf>,
}

/// One RFC 6962 log to follow.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogSection {
    /// A short label for alerts and state: lowercase letters, digits, `-`, and `_`.
    pub name: String,
    /// The log's base URL: its `/ct/v1/` endpoints are under it.
    pub url: String,
    /// The log's public key, base64 of its DER SubjectPublicKeyInfo (ECDSA P-256 or RSA), as the
    /// log lists publish it. Tree heads signed by any other key are refused.
    pub public_key: String,
    /// The first entry to read when the watcher has no state for this log. Without it, the
    /// watcher starts at the log's current end, so earlier entries are not read.
    pub start_index: Option<u64>,
}

/// Where alerts go: the node's `[alerts]` shape without the settings only the node uses.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchAlerts {
    pub outbox: OutboxSection,
    /// An alert that repeats with no resolve line pages again only after it has been quiet this
    /// long.
    #[serde(with = "humantime_serde")]
    pub episode_quiet: Duration,
    /// Where Critical alerts are posted, the only kind the watcher raises. Required unless the
    /// watcher runs with `--allow-mock`.
    pub critical: Option<WebhookRoute>,
}

/// Ten minutes, the time a restart's certificate may precede the attestation that names its key.
fn default_grace() -> Duration {
    Duration::from_secs(600)
}

/// An hour.
fn default_log_stale_after() -> Duration {
    Duration::from_secs(3600)
}

/// The vendor's key-distribution service.
fn default_kds() -> String {
    KDS_BASE_URL.to_owned()
}

impl WatchConfig {
    /// Loads and validates the config at `path`. `allow_mock` also accepts plain HTTP to a
    /// loopback address for the node and the logs, which only a dev setup uses.
    /// Error cases:
    /// - the file cannot be read or parsed, or a value fails its structural checks
    pub fn load(path: &Path, allow_mock: bool) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate(allow_mock).with_context(|| format!("validating {}", path.display()))?;
        Ok(cfg)
    }

    /// Checks the domain, the intervals, the node and the logs.
    /// Error cases:
    /// - the domain is not a lowercase hostname, an interval is zero or the attestation interval
    ///   exceeds half the grace, a URL is not HTTPS (or loopback HTTP with `allow_mock`) or carries
    ///   credentials, a log name repeats or is malformed, or a log key is not a supported one
    pub(super) fn validate(&self, allow_mock: bool) -> anyhow::Result<()> {
        validate_domain(&self.domain)?;
        ensure!(!self.state_file.as_os_str().is_empty(), "state_file is empty");
        for (name, interval) in [
            ("attestation_interval", self.attestation_interval),
            ("log_interval", self.log_interval),
            ("grace", self.grace),
            ("log_stale_after", self.log_stale_after),
        ] {
            ensure!(!interval.is_zero(), "{name} must be positive");
        }
        ensure!(
            self.attestation_interval.saturating_mul(2) <= self.grace,
            "attestation_interval must be at most half of grace, or a restart's new key can be missed"
        );
        ensure!(self.log_stale_after > self.log_interval, "log_stale_after must exceed log_interval");
        endpoint_url(&self.node.url, allow_mock).context("node.url")?;
        endpoint_url(&self.node.kds, false).context("node.kds")?;
        ensure!(!self.node.processor.is_empty(), "node.processor is empty");
        ensure!(!self.logs.is_empty(), "no [[logs]] to follow");
        let mut names = std::collections::BTreeSet::new();
        let mut keys = std::collections::BTreeSet::new();
        for log in &self.logs {
            let name_ok = !log.name.is_empty()
                && log.name.len() <= MAX_LOG_NAME_LEN
                && log.name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
            ensure!(name_ok, "log name {:?} must be 1-{MAX_LOG_NAME_LEN} of a-z, 0-9, '-', '_'", log.name);
            ensure!(names.insert(log.name.as_str()), "log {} is listed twice", log.name);
            endpoint_url(&log.url, allow_mock).with_context(|| format!("log {} url", log.name))?;
            let key = LogKey::parse(&log.public_key).with_context(|| format!("log {} public_key", log.name))?;
            ensure!(keys.insert(key.fingerprint().to_owned()), "log {} repeats another log's public_key", log.name);
        }
        self.alerts.outbox.validate().context("alerts.outbox")?;
        // Open alerts are asserted on every check, so the quiet interval must outlast the gap between
        // checks, or each assertion would page again.
        ensure!(
            self.alerts.episode_quiet > self.log_interval.max(self.attestation_interval),
            "alerts.episode_quiet must exceed log_interval and attestation_interval"
        );
        Ok(())
    }
}

/// Parses `url` as an endpoint base: HTTPS, or with `allow_http_loopback` plain HTTP to a loopback
/// address, with no credentials, query or fragment.
/// Error cases:
/// - not a URL, another scheme, a non-loopback HTTP host, or credentials, a query or a fragment
pub(super) fn endpoint_url(url: &str, allow_http_loopback: bool) -> anyhow::Result<Url> {
    let parsed = Url::parse(url).map_err(|e| anyhow::anyhow!("not a URL: {e}"))?;
    ensure!(
        parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "a URL here carries no credentials, query or fragment"
    );
    let loopback = parsed
        .host_str()
        .and_then(|host| host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback());
    match parsed.scheme() {
        "https" => {}
        "http" if allow_http_loopback && loopback => {}
        "http" => bail!("must be https (plain http to a loopback address is for --allow-mock only)"),
        scheme => bail!("unsupported scheme {scheme}"),
    }
    Ok(parsed)
}

/// Requires a lowercase ASCII hostname: dot-separated labels of letters, digits and hyphens, none
/// empty, over 63 bytes, or starting or ending with a hyphen.
/// Error cases:
/// - the domain is empty, too long, has such a label, or is a wildcard
fn validate_domain(domain: &str) -> anyhow::Result<()> {
    ensure!(!domain.is_empty() && domain.len() <= MAX_DOMAIN_LEN, "domain must be 1-{MAX_DOMAIN_LEN} bytes");
    for label in domain.split('.') {
        let ok = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        ensure!(ok, "domain {domain:?} must be a lowercase ASCII hostname (IDNs in punycode, no wildcard)");
    }
    Ok(())
}
