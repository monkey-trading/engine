//! The operator's telemetry config: the metrics listener and the alert webhooks.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail};
use reqwest::Url;
use serde::Deserialize;

use crate::config::SecretName;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsSection {
    /// Operator address for `GET /metrics` in Prometheus text format; keep it off the public API's
    /// interface.
    pub listen: SocketAddr,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertsSection {
    pub outbox: OutboxSection,
    /// An open market that a configured feed prices raises a Critical `market_no_price` alert after
    /// this long with no usable price, and a sequencer that answers no monitor sample for this long
    /// raises `sequencer_stalled`.
    #[serde(with = "humantime_serde")]
    pub no_price_after: Duration,
    /// An alert that repeats with no resolve line pages again only after it has been quiet this
    /// long.
    #[serde(with = "humantime_serde")]
    pub episode_quiet: Duration,
    /// Where Critical alerts are posted. Without it they are only logged.
    pub critical: Option<WebhookRoute>,
    /// Where Warning alerts are posted. Without it they are only logged.
    pub warning: Option<WebhookRoute>,
}

/// Durable private pages; this directory must be beside, never inside, the public log.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboxSection {
    pub dir: PathBuf,
    pub max_pages: u64,
    pub max_bytes: u64,
    #[serde(with = "humantime_serde")]
    pub max_age: Duration,
}

impl OutboxSection {
    /// Requires sibling directories so the outbox stays outside public archive inputs.
    /// Error cases:
    /// - paths overlap, use parent traversal, or have different data directories
    pub fn validate_location(&self, log: &Path) -> anyhow::Result<()> {
        let plain = |path: &Path| !path.components().any(|c| matches!(c, Component::ParentDir));
        anyhow::ensure!(plain(&self.dir) && plain(log), "outbox and log paths must not use parent traversal");
        let outbox = std::path::absolute(&self.dir)?;
        let log = std::path::absolute(log)?;
        anyhow::ensure!(
            outbox != log && outbox.parent() == log.parent(),
            "alerts.outbox.dir must be beside node.log_dir"
        );
        Ok(())
    }

    /// Rejects symlinks that would place private pages in the public log.
    /// Error cases:
    /// - creating the data parent fails, or existing paths resolve outside their configured parent
    pub fn check_disk_location(&self, log: &Path) -> anyhow::Result<()> {
        self.validate_location(log)?;
        let dir = std::path::absolute(&self.dir)?;
        let parent = dir.parent().expect("absolute outbox path has a parent");
        std::fs::create_dir_all(parent)?;
        let parent = parent.canonicalize()?;
        for path in [&self.dir, log] {
            if path.try_exists()? {
                anyhow::ensure!(
                    path.canonicalize()?.parent() == Some(parent.as_path()),
                    "outbox and log must resolve to sibling directories"
                );
            }
        }
        if self.dir.try_exists()? && log.try_exists()? {
            anyhow::ensure!(self.dir.canonicalize()? != log.canonicalize()?, "outbox overlaps public log");
        }
        Ok(())
    }

    /// Validates nonzero bounds and representable retention arithmetic.
    /// Error cases:
    /// - a bound is zero or exceeds SQLite's signed integer range
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.dir.as_os_str().is_empty(), "alerts.outbox.dir is empty");
        anyhow::ensure!(self.max_pages > 0 && self.max_pages <= i64::MAX as u64, "invalid alerts.outbox.max_pages");
        anyhow::ensure!(self.max_bytes > 0 && self.max_bytes <= i64::MAX as u64, "invalid alerts.outbox.max_bytes");
        anyhow::ensure!(
            self.max_age.as_millis() > 0 && self.max_age.as_millis() <= i64::MAX as u128,
            "invalid alerts.outbox.max_age"
        );
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookRoute {
    /// The environment variable holding the webhook URL, which is a credential and stays out of
    /// config files. A production node refuses it, since its guest has no environment to set it
    /// in; it names `url_secret` instead. The CT watcher, outside the enclave, reads only this.
    pub url_env: Option<String>,
    /// The delivered secret holding the webhook URL. Until it is delivered this route is off, and
    /// its alerts are only logged, under `secret_missing`.
    pub url_secret: Option<SecretName>,
}

/// A URL that may carry credentials. `Debug` hides it; read it with `expose` where it is used.
pub struct WebhookUrl(Url);

impl fmt::Debug for WebhookUrl {
    /// Never prints the URL.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebhookUrl(<redacted>)")
    }
}

impl WebhookRoute {
    /// A route that reads the variable `var`.
    pub fn from_env(var: &str) -> Self {
        Self { url_env: Some(var.to_owned()), url_secret: None }
    }
}

impl WebhookUrl {
    /// Reads the URL from the environment variable `var`, looked up through `env`.
    /// Error cases:
    /// - `var` is unset, or its value is not an acceptable URL (see `parse`)
    pub fn from_env(var: &str, env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let value = env(var).ok_or_else(|| anyhow!("environment variable {var} is not set"))?;
        Self::parse(var, &value)
    }

    /// `value`, the URL `source` holds. Plain HTTP is accepted only to this machine: the host
    /// controls the network.
    /// Error cases:
    /// - `value` is not a URL
    /// - the scheme is not HTTPS, unless it is HTTP to a loopback address
    pub(crate) fn parse(source: &str, value: &str) -> anyhow::Result<Self> {
        // `url::ParseError` names the problem, never the input, so nothing secret reaches a log.
        let url = Url::parse(value).map_err(|e| anyhow!("{source}: {e}"))?;
        let loopback = url
            .host_str()
            .and_then(|host| host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        match url.scheme() {
            "https" => Ok(Self(url)),
            "http" if loopback => Ok(Self(url)),
            _ => bail!("{source}: must be an https URL, or http to a loopback address"),
        }
    }

    /// The URL, for the one place that connects with it.
    pub fn expose(&self) -> &Url {
        &self.0
    }
}
