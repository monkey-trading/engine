//! `[tee.sealing]`: where the enclave's sealed keys live, and where this release hands them to its
//! successor.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::ensure;
use serde::Deserialize;

use crate::config::{NodeConfig, TeeProvider};

/// The enclave's keys sealed on the data disk, so a restart keeps them (see `tee::sealing`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealingSection {
    /// The directory of sealed key files, one per release.
    pub dir: PathBuf,
    /// Hands the keys to a successor release the vault names.
    pub handover: Option<HandoverSection>,
}

/// Where a successor release asks this one for its keys, and where this release finds the
/// predecessor's published log when an election names it the operator.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoverSection {
    /// This release's handover listener, which the release the vault names connects to for the
    /// keys.
    pub listen: SocketAddr,
    /// Archive mirrors (#48's) of the incumbent's published log, tried in order before the
    /// incumbent's own registered API hostname when this node takes the keys over an election.
    #[serde(default)]
    pub mirrors: Vec<String>,
    /// Accept the predecessor's self-signed or private-CA TLS certificate when fetching its
    /// archive. Development only: the enclave-keyed checkpoint inside the log, not the transport,
    /// is the authenticity boundary; a production build refuses this.
    #[serde(default)]
    pub danger_accept_invalid_certs: bool,
}

impl NodeConfig {
    /// Checks `[tee.sealing]`.
    /// Error cases:
    /// - it is set beside `tee.use_published_mock_key`, whose key is public
    /// - it is set with real attestation, or `[tee.sealing.handover]` is set, without
    ///   `[checkpoints]`, which reads the release the vault names
    pub(super) fn validate_sealing(&self) -> anyhow::Result<()> {
        let Some(sealing) = &self.tee.sealing else { return Ok(()) };
        ensure!(!self.tee.use_published_mock_key, "[tee.sealing] cannot seal tee.use_published_mock_key's public key");
        ensure!(
            self.tee.provider == TeeProvider::Mock || self.checkpoints.is_some(),
            "[tee.sealing] with real attestation needs [checkpoints]: the vault names the release that opens the keys"
        );
        ensure!(
            sealing.handover.is_none() || self.checkpoints.is_some(),
            "[tee.sealing.handover] needs [checkpoints]: the vault names the release that takes the keys"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::config::TeeProvider;
    use crate::test_support::dev_config;

    use super::*;

    fn sealing(text: &str) -> SealingSection {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn the_mock_tee_seals_without_a_vault_and_real_attestation_needs_one() {
        let mut cfg = dev_config();
        cfg.tee.use_published_mock_key = false;
        cfg.tee.sealing = Some(sealing("dir = 'keys'"));
        cfg.validate_sealing().unwrap();

        cfg.tee.provider = TeeProvider::ConfigfsTsm;
        assert!(cfg.checkpoints.is_none());
        let err = cfg.validate_sealing().unwrap_err();
        assert!(err.to_string().contains("real attestation needs [checkpoints]"), "{err:#}");
    }

    #[test]
    fn the_published_key_is_never_sealed_and_a_handover_needs_the_vault() {
        let mut cfg = dev_config();
        assert!(cfg.tee.use_published_mock_key, "the dev config uses the published key");
        cfg.tee.sealing = Some(sealing("dir = 'keys'"));
        let err = cfg.validate_sealing().unwrap_err();
        assert!(err.to_string().contains("public key"), "{err:#}");

        cfg.tee.use_published_mock_key = false;
        cfg.tee.sealing = Some(sealing("dir = 'keys'\n[handover]\nlisten = '127.0.0.1:9471'"));
        assert!(cfg.checkpoints.is_none());
        let err = cfg.validate_sealing().unwrap_err();
        assert!(err.to_string().contains("needs [checkpoints]"), "{err:#}");
    }

    #[test]
    fn the_stand_in_measurement_is_not_config() {
        let refused =
            toml::from_str::<SealingSection>(&format!("dir = 'keys'\nmock_launch_digest = '0x{}'", "11".repeat(48)));
        assert!(refused.is_err(), "the config names the stand-in measurement, so it would name the release twice");
    }
}
