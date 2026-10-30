//! The `[history.marks]` section: the derived marks cache's retention policy and its optional
//! provider backfill, with the validation that keeps either from failing silently.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use engine_core::types::Millis;
use serde::Deserialize;

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
