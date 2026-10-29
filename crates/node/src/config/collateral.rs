//! The `[tee]` keys that drive the attestation collateral relay.

use std::time::Duration;

use anyhow::{Context, ensure};
use reqwest::Url;

use crate::config::TeeSection;
use crate::verify::KDS_BASE_URL;

/// The bounds on `tee.collateral_refresh`: often enough to follow the revocation list, not so
/// often that a typo hammers the vendor.
const MIN_REFRESH: Duration = Duration::from_secs(60);
const MAX_REFRESH: Duration = Duration::from_secs(24 * 60 * 60);

/// The vendor's service, when the config names none.
pub(super) fn default_kds_base() -> String {
    KDS_BASE_URL.to_owned()
}

/// Twelve hours: well inside a list's validity, and a few fetches a day at most.
pub(super) fn default_refresh() -> Duration {
    Duration::from_secs(12 * 60 * 60)
}

impl TeeSection {
    /// Checks the relay's keys.
    /// Error cases:
    /// - `collateral_refresh` is outside one minute to one day
    /// - `processor` is empty or not alphanumeric, since it becomes a URL path segment
    /// - `kds_base` is not an HTTPS URL without query or fragment
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            (MIN_REFRESH..=MAX_REFRESH).contains(&self.collateral_refresh),
            "tee.collateral_refresh must be between 1m and 24h"
        );
        if let Some(processor) = &self.processor {
            ensure!(
                !processor.is_empty() && processor.chars().all(|c| c.is_ascii_alphanumeric()),
                "tee.processor must be alphanumeric"
            );
        }
        let url = Url::parse(&self.kds_base).context("tee.kds_base")?;
        ensure!(url.scheme() == "https", "tee.kds_base must be an https URL");
        ensure!(url.query().is_none() && url.fragment().is_none(), "tee.kds_base must not carry a query or fragment");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tee(extra: &str) -> TeeSection {
        toml::from_str(&format!("provider = 'configfs_tsm'\n{extra}")).unwrap()
    }

    #[test]
    fn defaults_name_the_vendor_service_and_refresh_every_twelve_hours() {
        let section = tee("");
        assert_eq!(section.kds_base, KDS_BASE_URL);
        assert_eq!(section.collateral_refresh, Duration::from_secs(12 * 60 * 60));
        assert_eq!(section.processor, None);
        section.validate().expect("the defaults are valid");
    }

    #[test]
    fn validation_bounds_the_refresh_the_processor_and_the_service_url() {
        for bad in [
            "collateral_refresh = '30s'",
            "collateral_refresh = '25h'",
            "processor = ''",
            "processor = 'Milan/../x'",
            "kds_base = 'http://kds.example'",
            "kds_base = 'http://127.0.0.1:9'",
            "kds_base = 'https://kds.example/?a=1'",
            "kds_base = 'not a url'",
        ] {
            assert!(tee(bad).validate().is_err(), "accepted {bad}");
        }
        tee("processor = 'Genoa'\nkds_base = 'https://kds.example'\ncollateral_refresh = '1m'")
            .validate()
            .expect("a valid relay config");
    }
}
