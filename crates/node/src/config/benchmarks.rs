//! Public publisher endpoints for the separate operator benchmark worker.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{Context, ensure};
use reqwest::Url;
use serde::Deserialize;

use crate::config::{NodeConfig, TeeProvider};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Publisher {
    Sofr,
    Estr,
    Sonia,
    Aonia,
    Tona,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSource {
    pub publisher: Publisher,
    /// The official publisher's own HTTPS host on port 443, with no credentials or fragment, read
    /// against compiled-in roots with no proxy or redirect; loopback HTTP only under the mock TEE.
    pub endpoint: String,
}

/// `[benchmarks]`: the publishers the separate worker (`engine admin sync-benchmarks`) fetches dated
/// rates from. With it, no rollover after a start is charged until a signed snapshot has attempted
/// every listed benchmark: the start holds ordinary sequencing for it when it finds a rollover unpaid,
/// or when its next rollover comes due first (`benchmark_sync_pending`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSection {
    /// How often a watching worker fetches and signs a snapshot, at most 24 hours; it also fetches
    /// before each rollover, and retries a failed or refused submission within the shorter of this
    /// and the node-call timeout.
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// Each publisher fetch's deadline. The fetch before a rollover starts early enough for one per
    /// source and three calls to the node.
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    /// One official publisher per benchmark, covering every genesis market's benchmark.
    pub sources: Vec<BenchmarkSource>,
}

impl Publisher {
    /// The catalogue's benchmark name.
    pub fn benchmark(self) -> &'static str {
        match self {
            Self::Sofr => "SOFR",
            Self::Estr => "ESTR",
            Self::Sonia => "SONIA",
            Self::Aonia => "AONIA",
            Self::Tona => "TONA",
        }
    }

    /// The official publisher's HTTPS host.
    fn host(self) -> &'static str {
        match self {
            Self::Sofr => "markets.newyorkfed.org",
            Self::Estr => "data-api.ecb.europa.eu",
            Self::Sonia => "www.bankofengland.co.uk",
            Self::Aonia => "www.rba.gov.au",
            Self::Tona => "www.stat-search.boj.or.jp",
        }
    }
}

impl BenchmarkSection {
    /// Checks cadence, endpoint trust, and coverage of the genesis catalogue.
    /// Error cases:
    /// - zero/over-daily cadence, zero timeout, duplicate/missing publisher, or nonofficial endpoint
    pub(super) fn validate(&self, cfg: &NodeConfig) -> anyhow::Result<()> {
        ensure!(
            !self.interval.is_zero() && self.interval <= Duration::from_secs(86_400),
            "benchmarks.interval must be within (0, 24h]"
        );
        ensure!(!self.timeout.is_zero(), "benchmarks.timeout must be positive");
        let mut names = BTreeSet::new();
        for source in &self.sources {
            ensure!(names.insert(source.publisher.benchmark()), "duplicate benchmark publisher");
            let url = Url::parse(&source.endpoint).context("benchmark endpoint is not a URL")?;
            ensure!(
                url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
                "benchmark endpoints cannot carry credentials or fragments"
            );
            let official = url.scheme() == "https"
                && url.host_str() == Some(source.publisher.host())
                && url.port_or_known_default() == Some(443);
            let fixture =
                cfg.tee.provider == TeeProvider::Mock && url.scheme() == "http" && url.host_str() == Some("127.0.0.1");
            ensure!(
                official || fixture,
                "benchmark endpoint must use its official HTTPS publisher (loopback HTTP is mock-only)"
            );
        }
        for market in &cfg.genesis.markets {
            ensure!(
                names.contains(market.financing.benchmark.as_str()),
                "no publisher configured for {}",
                market.financing.benchmark
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::dev_config;

    #[test]
    fn official_publisher_config_covers_the_catalogue_and_refuses_invalid_endpoints() {
        let cfg = dev_config();
        #[derive(Deserialize)]
        struct File {
            benchmarks: BenchmarkSection,
        }
        let mut settings =
            toml::from_str::<File>(include_str!("../../../../config/benchmark-publishers.toml")).unwrap().benchmarks;
        settings.validate(&cfg).unwrap();
        let official = settings.sources[0].endpoint.clone();
        for endpoint in [
            "http://markets.newyorkfed.org/api",
            "https://evil.example/api",
            "https://markets.newyorkfed.org.evil.example/api",
            "https://user:password@markets.newyorkfed.org/api",
        ] {
            settings.sources[0].endpoint = endpoint.into();
            assert!(settings.validate(&cfg).is_err(), "{endpoint}");
        }
        settings.sources[0].endpoint = official;
        settings
            .sources
            .push(BenchmarkSource { publisher: Publisher::Sofr, endpoint: settings.sources[0].endpoint.clone() });
        assert!(settings.validate(&cfg).is_err());
        settings.sources.pop();
        settings.interval = Duration::ZERO;
        assert!(settings.validate(&cfg).is_err());
        settings.interval = Duration::from_secs(86_401);
        assert!(settings.validate(&cfg).is_err());
        settings.interval = Duration::from_secs(3600);
        settings.timeout = Duration::ZERO;
        assert!(settings.validate(&cfg).is_err());
    }

    #[test]
    fn loopback_publishers_are_only_accepted_for_mock_tees() {
        let mut cfg = dev_config();
        let settings = BenchmarkSection {
            interval: Duration::from_secs(3600),
            timeout: Duration::from_secs(20),
            sources: vec![
                BenchmarkSource { publisher: Publisher::Sofr, endpoint: "http://127.0.0.1:8000/sofr".into() },
                BenchmarkSource { publisher: Publisher::Estr, endpoint: "http://127.0.0.1:8000/estr".into() },
            ],
        };
        settings.validate(&cfg).unwrap();
        cfg.tee.provider = TeeProvider::ConfigfsTsm;
        assert!(settings.validate(&cfg).is_err());
    }
}
