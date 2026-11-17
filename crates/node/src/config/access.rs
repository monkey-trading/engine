//! Bounded access-source configuration. Legal policy and provider selection remain operator inputs.

use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::time::Duration;

use anyhow::{Context, ensure};
use engine_core::access::AccessPolicy;
use engine_core::types::AccountId;
use serde::Deserialize;

use crate::config::SecretUrl;

/// The most screening slots a production build lets one account's requests, and one client's, hold
/// at once, and the fewest it must keep for closes and withdrawals.
const PRODUCTION_PER_ACCOUNT: usize = 2;
const PRODUCTION_PER_CLIENT: usize = 4;
const PRODUCTION_RESERVED: usize = 2;

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
    /// The API's screening slots: requests waiting on the source at once, in all.
    pub max_in_flight: NonZeroUsize,
    /// How many of them one account's requests may hold, its session keys' included.
    pub max_in_flight_per_account: NonZeroUsize,
    /// How many of them one client's requests may hold, for every account it acts for.
    pub max_in_flight_per_client: NonZeroUsize,
    /// How many of them only closes, close-alls, take-profit and stop-loss requests, withdrawals and
    /// payout reissues may take.
    pub reserved_in_flight: usize,
}

impl AccessSection {
    /// Checks structural bounds and refuses plaintext network transport outside loopback.
    /// Error cases:
    /// - an invalid URL, insecure remote transport, zero bound, or oversized response bound
    /// - fewer than two screening slots, which would let a declaration hold every one an order needs
    /// - a reserve of half the slots or more, which would leave orders none while declarations hold
    ///   theirs
    pub fn validate(&self) -> anyhow::Result<()> {
        let url = self.endpoint.attested().context("access.endpoint")?;
        let loopback = url
            .host_str()
            .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && loopback),
            "access.endpoint requires HTTPS or loopback HTTP"
        );
        ensure!(!self.timeout.is_zero(), "access.timeout must be positive");
        ensure!(self.max_in_flight.get() >= 2, "access.max_in_flight must be at least 2 to leave orders a slot");
        ensure!(
            self.reserved_in_flight < self.max_in_flight.get() / 2,
            "access.reserved_in_flight must be below half of access.max_in_flight to leave orders a slot"
        );
        ensure!(
            self.max_age.as_millis() > 0 && self.max_age.as_millis() <= i64::MAX as u128,
            "access.max_age is out of range"
        );
        ensure!(self.source != AccountId([0; 20]), "access.source must be nonzero");
        ensure!((1..=65_536).contains(&self.max_response_bytes), "access.max_response_bytes must be 1..=65536");
        Ok(())
    }

    /// Refuses, when `production` is set, screening shares above the production values or a reserve
    /// below it, so no one account or client holds more than its share and closes and withdrawals
    /// keep slots that orders cannot take.
    /// Error cases:
    /// - `production` is set and a share is above its production value, or the reserve below its own
    pub fn refuse_dev_screening_shares(&self, production: bool) -> anyhow::Result<()> {
        if !production {
            return Ok(());
        }
        for (key, share, ceiling) in [
            ("max_in_flight_per_account", self.max_in_flight_per_account.get(), PRODUCTION_PER_ACCOUNT),
            ("max_in_flight_per_client", self.max_in_flight_per_client.get(), PRODUCTION_PER_CLIENT),
        ] {
            ensure!(
                share <= ceiling,
                "a production build allows access.{key} at most {ceiling}, and the config sets {share}"
            );
        }
        ensure!(
            self.reserved_in_flight >= PRODUCTION_RESERVED,
            "a production build needs access.reserved_in_flight at least {PRODUCTION_RESERVED}, and the config sets {}",
            self.reserved_in_flight
        );
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

    const VALID: &str = "source='0x1111111111111111111111111111111111111111'\nendpoint='https://screen.example/api?credential=private-value'\ntimeout='1s'\nmax_age='1s'\nmax_response_bytes=4096\nmax_in_flight=2\nmax_in_flight_per_account=1\nmax_in_flight_per_client=1\nreserved_in_flight=0";

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
        let single: AccessSection = toml::from_str(&VALID.replace("max_in_flight=2", "max_in_flight=1")).unwrap();
        assert!(single.validate().unwrap_err().to_string().contains("access.max_in_flight"));
        assert!(toml::from_str::<AccessSection>(&format!("{VALID}\nmax_ages='2s'")).is_err());
        for share in ["max_in_flight_per_account=1", "max_in_flight_per_client=1", "reserved_in_flight=0"] {
            assert!(toml::from_str::<AccessSection>(&VALID.replace(&format!("\n{share}"), "")).is_err(), "{share}");
        }
        assert!(toml::from_str::<AccessSection>(&VALID.replace("per_client=1", "per_client=0")).is_err());
    }

    #[test]
    fn a_reserve_must_leave_orders_a_slot_beside_the_declarations() {
        let reserving = |slots: usize, reserved: usize| {
            toml::from_str::<AccessSection>(
                &VALID
                    .replace("max_in_flight=2", &format!("max_in_flight={slots}"))
                    .replace("reserved_in_flight=0", &format!("reserved_in_flight={reserved}")),
            )
            .unwrap()
            .validate()
        };
        // Declarations hold up to half the slots, rounded up; orders keep one more than the reserve.
        for (slots, most) in [(2, 0), (3, 0), (4, 1), (5, 1), (8, 3), (9, 3)] {
            reserving(slots, most).unwrap();
            let error = reserving(slots, most + 1).unwrap_err().to_string();
            assert!(error.contains("access.reserved_in_flight"), "{slots}: {error}");
        }
    }

    // Runs in the production lane too, which filters on `production_build`.
    #[test]
    fn a_production_build_refuses_screening_shares_beyond_its_values() {
        let config = |account: usize, client: usize, reserved: usize| {
            toml::from_str::<AccessSection>(
                &VALID
                    .replace("max_in_flight=2", "max_in_flight=8")
                    .replace("per_account=1", &format!("per_account={account}"))
                    .replace("per_client=1", &format!("per_client={client}"))
                    .replace("reserved_in_flight=0", &format!("reserved_in_flight={reserved}")),
            )
            .unwrap()
        };
        let at_values = config(2, 4, 2);
        at_values.validate().unwrap();
        at_values.refuse_dev_screening_shares(true).unwrap();
        for (beyond, key) in [
            (config(3, 4, 2), "max_in_flight_per_account at most 2"),
            (config(2, 5, 2), "max_in_flight_per_client at most 4"),
            (config(2, 4, 1), "reserved_in_flight at least 2"),
        ] {
            beyond.validate().unwrap();
            beyond.refuse_dev_screening_shares(false).unwrap();
            let error = beyond.refuse_dev_screening_shares(true).unwrap_err().to_string();
            assert!(error.contains(key), "{key}: {error}");
        }
    }
}
