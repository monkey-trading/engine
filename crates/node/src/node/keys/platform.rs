//! The platform a start runs on: the source of its quotes, and the sealing its keys are sealed under.

#[cfg(not(target_os = "linux"))]
use anyhow::bail;
use anyhow::{Context, ensure};
use tracing::warn;

use super::KeyStart;
use crate::config::TeeProvider;
use crate::tee::clock::GuestClock;
use crate::tee::sealing::{MockSealer, Sealer};
use crate::tee::{Attestor, ConfigfsTsm, MockAttestor};

/// The source of `provider`'s quotes. With real attestation nothing is attested before the guest
/// clock proves protected (`image/README.md`, "The guest's clock").
/// Error cases:
/// - with `ConfigfsTsm`, `clock` is not the TSC in the frequency the hardware reports
pub(in crate::node) fn attestor_for(provider: TeeProvider, clock: &GuestClock) -> anyhow::Result<Box<dyn Attestor>> {
    match provider {
        TeeProvider::Mock => Ok(Box::new(MockAttestor)),
        TeeProvider::ConfigfsTsm => {
            clock.require_protected()?;
            Ok(Box::new(ConfigfsTsm::default()))
        }
    }
}

/// The platform's sealing for `provider`.
/// Error cases:
/// - the mock TEE has no stand-in measurement, real attestation is given one, or SEV-SNP sealing is
///   asked for off Linux
pub(in crate::node) fn sealer(provider: TeeProvider, start: &KeyStart) -> anyhow::Result<Box<dyn Sealer>> {
    if provider == TeeProvider::Mock {
        let launch_digest = start
            .mock_launch_digest
            .context("the mock TEE's stand-in for sealing needs --mock-launch-digest, its stand-in measurement")?;
        warn!(launch_digest = %hex::encode(launch_digest), "mock TEE: the keys' sealing is a public stand-in, not sealing");
        return Ok(Box::new(MockSealer { launch_digest }));
    }
    ensure!(
        start.mock_launch_digest.is_none(),
        "--mock-launch-digest is for the mock TEE: the platform measures this release"
    );
    #[cfg(target_os = "linux")]
    return Ok(Box::new(crate::tee::snp::SnpSealer));
    #[cfg(not(target_os = "linux"))]
    bail!("SEV-SNP sealing needs a Linux guest")
}
