//! SHA-256 catalog prefix commitments; tree proofs remain entirely in the JMT adapter.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::log::catalog::{CheckpointLocator, DenseSegmentInfo};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocatorCommitment {
    pub count: u64,
    pub sha256: [u8; 32],
}

/// Commits all exact locators, including their order and physical framing.
/// Error cases:
/// - a locator cannot be encoded or the list is duplicated/out of physical order
pub(super) fn locator_commitment(locators: &[CheckpointLocator]) -> anyhow::Result<LocatorCommitment> {
    let mut hash = Sha256::new();
    hash.update(b"engine.catalog.locators.v2\0");
    hash.update((locators.len() as u64).to_be_bytes());
    let mut previous: Option<&CheckpointLocator> = None;
    for locator in locators {
        if let Some(prev) = previous {
            ensure!(prev.offset < locator.offset && prev.end <= locator.start, "checkpoint locator order");
        }
        let bytes = serde_json::to_vec(locator)?;
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
        previous = Some(locator);
    }
    Ok(LocatorCommitment { count: locators.len() as u64, sha256: hash.finalize().into() })
}

/// Extends the completed prefix with exact manifest bytes, ordinal, boundary and all locators.
pub(super) fn extend_catalog_prefix(previous: [u8; 32], dense: &DenseSegmentInfo) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"engine.catalog.prefix.v2\0");
    hash.update(previous);
    hash.update(dense.ordinal.to_be_bytes());
    hash.update(dense.manifest_offset.to_be_bytes());
    hash.update((dense.manifest_line.len() as u64).to_be_bytes());
    hash.update(&dense.manifest_line);
    hash.update(dense.locators.count.to_be_bytes());
    hash.update(dense.locators.sha256);
    hash.finalize().into()
}

/// Constructs a committed dense completion from the exact writer/replay bytes.
/// Error cases:
/// - encoding, arithmetic, descriptor validation, or locator validation fails
pub(super) fn dense_completion(
    ordinal: u64,
    manifest_offset: u64,
    previous_prefix: [u8; 32],
    info: crate::log::SegmentInfo,
    checkpoints: &[CheckpointLocator],
) -> anyhow::Result<DenseSegmentInfo> {
    let mut dense = DenseSegmentInfo {
        ordinal,
        manifest_offset,
        manifest_line: crate::log::segments::listing_line(&info)?,
        info,
        previous_prefix,
        prefix: [0; 32],
        locators: locator_commitment(checkpoints)?,
    };
    for locator in checkpoints {
        crate::log::catalog::validate_locator(locator, locator.offset)?;
        ensure!(
            locator.segment_first_seq == dense.info.first_seq
                && locator.end <= dense.info.bytes
                && locator.offset.seq <= dense.info.last_seq,
            "checkpoint outside completed segment"
        );
    }
    dense.prefix = extend_catalog_prefix(previous_prefix, &dense);
    dense.manifest_offset.checked_add(dense.manifest_line.len() as u64).context("manifest overflow")?;
    crate::log::catalog::validate_dense(&dense, ordinal)?;
    Ok(dense)
}
