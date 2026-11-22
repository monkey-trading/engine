//! The `[tee]` section: the attestation provider, the keys that drive the attestation collateral
//! relay, and the CPU models a peer's quote is held to.

use std::time::Duration;

use anyhow::{Context, ensure};
use engine_verify::CpuModel;
use reqwest::Url;
use serde::Deserialize;

use crate::config::{SealingSection, TeeProvider};
use crate::verify::KDS_BASE_URL;

/// The bounds on `tee.collateral_refresh`: often enough to follow the revocation list, not so
/// often that a typo hammers the vendor.
const MIN_REFRESH: Duration = Duration::from_secs(60);
const MAX_REFRESH: Duration = Duration::from_secs(24 * 60 * 60);

/// The vendor's service, when the config names none.
fn default_kds_base() -> String {
    KDS_BASE_URL.to_owned()
}

/// Twelve hours: well inside a list's validity, and a few fetches a day at most.
fn default_refresh() -> Duration {
    Duration::from_secs(12 * 60 * 60)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeeSection {
    pub provider: TeeProvider,
    /// Uses the public development signer only with simulated attestation.
    #[serde(default)]
    pub use_published_mock_key: bool,
    /// The vendor's product name, which keys its certificate chain: Milan, or Genoa for a Genoa,
    /// Bergamo or Siena processor. Required with an SEV-SNP quote; the node relays that quote's
    /// collateral, whose VCEK must name this product line.
    pub processor: Option<String>,
    /// Each CPU model a peer enclave's quote may come from, with the lowest TCB taken from that
    /// model: the release manifest's `cpu_models`, which a handover, a standby vouch and a log check
    /// hold a hardware peer to. Required with `configfs_tsm`.
    #[serde(default)]
    pub cpu_models: Vec<CpuModel>,
    /// The vendor's key-distribution service the node fetches collateral from.
    #[serde(default = "default_kds_base")]
    pub kds_base: String,
    /// How often the node refetches the collateral.
    #[serde(default = "default_refresh", with = "humantime_serde")]
    pub collateral_refresh: Duration,
    /// Seals the enclave's keys on the data disk, so a restart keeps them and the next release can
    /// take them over. A hardware enclave refuses to start without it.
    pub sealing: Option<SealingSection>,
}

impl TeeSection {
    /// Checks the relay's keys and the CPU models.
    /// Error cases:
    /// - `collateral_refresh` is outside one minute to one day
    /// - `processor` is empty or not alphanumeric, since it becomes a URL path segment
    /// - `kds_base` is not an HTTPS URL without query or fragment
    /// - `cpu_models` is empty with `configfs_tsm`, which would refuse every hardware peer, or fails
    ///   `CpuModel::validate_list`
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
        if self.provider == TeeProvider::ConfigfsTsm || !self.cpu_models.is_empty() {
            CpuModel::validate_list(&self.cpu_models, "tee.cpu_models")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hardware `[tee]` listing the Siena model, with `extra` keys.
    fn tee(extra: &str) -> TeeSection {
        toml::from_str(&format!("provider = 'configfs_tsm'\n{extra}\n{SIENA}")).unwrap()
    }

    const SIENA: &str = "[[cpu_models]]\nproduct_name = 'Genoa-A2'\nfamily = 0x19\nmodel = 0xA0\nstepping = 0x02\n\
                         minimum_tcb = { boot_loader = 0, tee = 0, snp = 0x1D, microcode = 0x1B }";

    #[test]
    fn a_hardware_enclave_needs_a_valid_cpu_model_list_and_a_mock_one_none() {
        let listed = tee("");
        assert_eq!(listed.cpu_models[0].product_name, "Genoa-A2");
        listed.validate().expect("the Siena model");
        let unlisted: TeeSection = toml::from_str("provider = 'configfs_tsm'").unwrap();
        let err = unlisted.validate().unwrap_err();
        assert_eq!(err.to_string(), "tee.cpu_models lists no CPU model, so it would accept no quote");
        let mock: TeeSection = toml::from_str("provider = 'mock'").unwrap();
        mock.validate().expect("a mock enclave takes no hardware peer");
        let wrong_line = tee("").cpu_models[0].clone();
        let wrong_line =
            TeeSection { cpu_models: vec![CpuModel { product_name: "Milan-B0".into(), ..wrong_line }], ..mock };
        let err = wrong_line.validate().unwrap_err();
        assert!(err.to_string().ends_with("is certified under Genoa"), "{err:#}");
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
