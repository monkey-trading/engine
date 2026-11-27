//! The operator's admission of a successor enclave, as `engine admin sign-admission` writes it.

use std::path::Path;

use alloy::primitives::Address;
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;

use crate::handover::admission::{ADMISSION_FILE, Admission};
use crate::log::CheckpointDomain;
use crate::standby_key::StandbyKeys;
use crate::tee::sealing::{MockSealer, Release};

/// Writes `signer`'s admission, for the scenario's chain and `vault`, of the standby key that an
/// enclave running `release` seals in its sealing directory `dir`, which its first take seals.
/// Error cases:
/// - the standby key cannot be opened or sealed, or the admission cannot be written
pub(super) fn admit(dir: &Path, release: Release, signer: &SigningKey, vault: Address) -> anyhow::Result<()> {
    let sealer = Box::new(MockSealer { launch_digest: release.launch_digest });
    let key = StandbyKeys::open_or_seal(dir, sealer, release.config_sha256)?.address();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId(vault.into()) };
    let admission = Admission::sign(signer, domain, key, release.id());
    std::fs::write(dir.join(ADMISSION_FILE), serde_json::to_vec(&admission)?)?;
    Ok(())
}
