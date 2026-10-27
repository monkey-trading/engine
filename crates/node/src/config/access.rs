//! Bounded access-source configuration. Legal policy and provider selection remain operator inputs.

use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::time::Duration;

use anyhow::{Context, ensure};
use engine_core::access::AccessPolicy;
use engine_core::types::AccountId;
use serde::Deserialize;
use tokio::sync::Semaphore;

use crate::config::SecretUrl;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessSection {
    pub source: AccountId,
    /// Credential-bearing URLs are redacted from diagnostics and never enter the public log.
    pub endpoint: SecretUrl,
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub max_age: Duration,
    pub max_response_bytes: usize,
    pub max_in_flight: NonZeroUsize,
}

impl AccessSection {
    /// Checks structural bounds and refuses plaintext network transport outside loopback.
    /// Error cases:
    /// - an invalid URL, insecure remote transport, zero bound, or oversized response bound
    pub fn validate(&self) -> anyhow::Result<()> {
        let url = self.endpoint.expose().context("access.endpoint")?;
        let loopback = url
            .host_str()
            .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && loopback),
            "access.endpoint requires HTTPS or loopback HTTP"
        );
        ensure!(!self.timeout.is_zero(), "access.timeout must be positive");
        ensure!(self.max_in_flight.get() <= Semaphore::MAX_PERMITS, "access.max_in_flight exceeds semaphore limit");
        ensure!(
            self.max_age.as_millis() > 0 && self.max_age.as_millis() <= i64::MAX as u128,
            "access.max_age is out of range"
        );
        ensure!(self.source != AccountId([0; 20]), "access.source must be nonzero");
        ensure!((1..=65_536).contains(&self.max_response_bytes), "access.max_response_bytes must be 1..=65536");
        Ok(())
    }

    /// The source identity and freshness bound replay uses, without credentials or endpoint.
    pub fn policy(&self) -> AccessPolicy {
        AccessPolicy { source: self.source, max_age_ms: self.max_age.as_millis() as i64 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "source='0x1111111111111111111111111111111111111111'\nendpoint='https://screen.example/api?credential=private-value'\ntimeout='1s'\nmax_age='1s'\nmax_response_bytes=4096\nmax_in_flight=2";

    #[test]
    fn oversized_screening_capacity_is_rejected_before_client_construction() {
        let mut config: AccessSection = toml::from_str(VALID).unwrap();
        config.max_in_flight = NonZeroUsize::new(Semaphore::MAX_PERMITS).unwrap();
        config.validate().unwrap();
        config.max_in_flight = NonZeroUsize::new(Semaphore::MAX_PERMITS + 1).unwrap();
        let Err(error) = crate::access::AccessClient::new(&config) else {
            panic!("invalid capacity must be a configuration error")
        };
        assert!(error.to_string().contains("access.max_in_flight"), "{error}");
    }

    #[test]
    fn access_config_refuses_unknown_fields_invalid_transports_and_unbounded_or_zero_limits() {
        let valid: AccessSection = toml::from_str(VALID).unwrap();
        valid.validate().unwrap();
        assert!(!format!("{valid:?}").contains("private-value"));
        assert_eq!(valid.policy().max_age_ms, 1000);
        for (from, to) in [
            ("https://screen.example/api?credential=private-value", "http://screen.example/api"),
            ("timeout='1s'", "timeout='0s'"),
            ("max_age='1s'", "max_age='0s'"),
            ("max_response_bytes=4096", "max_response_bytes=0"),
            ("max_response_bytes=4096", "max_response_bytes=65537"),
        ] {
            let config: AccessSection = toml::from_str(&VALID.replace(from, to)).unwrap();
            assert!(config.validate().is_err(), "{from} -> {to}");
        }
        assert!(toml::from_str::<AccessSection>(&VALID.replace("max_in_flight=2", "max_in_flight=0")).is_err());
        assert!(toml::from_str::<AccessSection>(&format!("{VALID}\nmax_ages='2s'")).is_err());
    }
}
