//! Authenticated dense segment catalog. Only explicit listing enumerates historical ordinals.

use std::fmt;
use std::sync::Arc;

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::authenticated::Root;
use crate::log::catalog_access::{CatalogPin, CatalogRead};
use crate::log::catalog_owner::CatalogOwner;
pub use crate::log::catalog_prefix::LocatorCommitment;
use crate::log::catalog_prefix::{dense_completion, extend_catalog_prefix};
use crate::log::segments::{listing_line, segment_name};
use crate::log::{Checkpoint, Offset, SegmentInfo};

pub(super) const META: &[u8] = b"log/catalog/v1/meta";
pub(super) const MAX_LIVE_SEGMENTS: usize = 64;
const MAX_LEAF_BYTES: usize = 64 * 1024;

/// Fixed certificate fields; the authenticated metadata leaf must agree with all of them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRoot {
    pub root: Root,
    pub completed_segments: u64,
    pub manifest_bytes: u64,
    pub next_seq: u64,
    pub prefix: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CatalogMeta {
    pub completed_segments: u64,
    pub manifest_bytes: u64,
    pub next_seq: u64,
    pub prefix: [u8; 32],
}

/// A dense ordinal and the exact canonical public manifest line at its byte boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DenseSegmentInfo {
    pub ordinal: u64,
    pub manifest_offset: u64,
    pub info: SegmentInfo,
    pub manifest_line: Vec<u8>,
    pub previous_prefix: [u8; 32],
    pub prefix: [u8; 32],
    pub locators: LocatorCommitment,
}

/// The physical framing and exact contents of one checkpoint, indexed by its log offset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointLocator {
    pub segment_first_seq: u64,
    pub offset: Offset,
    pub start: u64,
    pub end: u64,
    pub checkpoint: Checkpoint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Completion {
    pub dense: DenseSegmentInfo,
    pub checkpoints: Arc<[CheckpointLocator]>,
}

/// A canonical public listing already proven against some catalog root: its bytes, how many
/// segments it lists, and the sequence and commitment after them.
#[derive(Clone, Default)]
pub struct CatalogListing {
    pub bytes: Arc<[u8]>,
    count: u64,
    next_seq: u64,
    prefix: [u8; 32],
}

/// Immutable pinned root plus a bounded delta supplied only by verified replay or the writer.
#[derive(Clone)]
pub struct LazyCatalogView {
    pub(super) read: Arc<dyn CatalogRead>,
    pub(super) certified: CatalogRoot,
    pub(super) delta: Arc<[Completion]>,
    // Erasure keeps the catalog independent of the proof adapter's concrete lease guard type.
    pub(super) lease: CatalogPin,
}

impl fmt::Debug for LazyCatalogView {
    /// Prints identities, without requiring storage connections to implement Debug.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LazyCatalogView").field("certified", &self.certified).field("delta", &self.delta).finish()
    }
}

impl PartialEq for LazyCatalogView {
    /// Catalog equality compares authority and trusted deltas, never host storage contents.
    fn eq(&self, other: &Self) -> bool {
        self.certified == other.certified && self.delta == other.delta
    }
}
impl Eq for LazyCatalogView {}

impl LazyCatalogView {
    /// Validates fixed certificate fields with one authenticated point read.
    /// Error cases:
    /// - metadata is absent, malformed, or inconsistent with the certificate
    pub fn open(read: Arc<dyn CatalogRead>, certified: CatalogRoot) -> anyhow::Result<Self> {
        let lease = read.pin(&certified.root)?;
        let meta: CatalogMeta = required(read.as_ref(), &certified.root, META)?;
        ensure!(meta == certified.meta(), "catalog certificate metadata mismatch");
        ensure!(
            meta.completed_segments <= meta.next_seq && meta.completed_segments <= u64::MAX - MAX_LIVE_SEGMENTS as u64,
            "catalog count range"
        );
        ensure!(
            meta.completed_segments != 0 || (meta.manifest_bytes == 0 && meta.next_seq == 0 && meta.prefix == [0; 32]),
            "empty catalog metadata"
        );
        Ok(Self { read, certified, delta: Arc::from([]), lease })
    }

    /// Whether another completion requires a durable fold first.
    pub(super) fn live_full(&self) -> bool {
        self.delta.len() == MAX_LIVE_SEGMENTS
    }

    /// The fixed durable root. Live completions are deliberately not included here.
    pub fn certified(&self) -> &CatalogRoot {
        &self.certified
    }

    /// The number of completed segments, including trusted live completions.
    pub fn len(&self) -> u64 {
        self.certified.completed_segments + self.delta.len() as u64
    }

    /// Whether this catalog has no completed segments.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The canonical manifest byte boundary, including trusted live completions.
    pub fn manifest_bytes(&self) -> u64 {
        self.delta
            .last()
            .map_or(self.certified.manifest_bytes, |c| c.dense.manifest_offset + c.dense.manifest_line.len() as u64)
    }

    /// Cumulative completed-prefix identity, independent of JMT versions and fold batch sizes.
    pub fn prefix(&self) -> [u8; 32] {
        self.delta.last().map_or(self.certified.prefix, |c| c.dense.prefix)
    }

    /// The first seq after all completed segments.
    pub fn next_seq(&self) -> u64 {
        self.delta.last().map_or(self.certified.next_seq, |c| c.dense.info.last_seq + 1)
    }

    /// Reads one dense ordinal and rejects a missing required rank.
    /// Error cases:
    /// - the proof, canonical bytes, range, or required ordinal is invalid
    pub fn at(&self, ordinal: u64) -> anyhow::Result<DenseSegmentInfo> {
        ensure!(ordinal < self.len(), "catalog ordinal out of range");
        let dense = if ordinal < self.certified.completed_segments {
            required(self.read.as_ref(), &self.certified.root, &ordinal_key(ordinal))?
        } else {
            self.delta[(ordinal - self.certified.completed_segments) as usize].dense.clone()
        };
        validate_dense(&dense, ordinal)?;
        ensure!(
            dense
                .manifest_offset
                .checked_add(dense.manifest_line.len() as u64)
                .is_some_and(|end| end <= self.manifest_bytes()),
            "catalog line outside manifest boundary"
        );
        Ok(dense)
    }

    /// Finds the segment containing seq using at most 64 verified ordinal probes.
    /// Error cases:
    /// - any required proof/rank fails, or a certified range contains a gap
    pub fn lookup_seq(&self, seq: u64) -> anyhow::Result<Option<DenseSegmentInfo>> {
        if seq >= self.next_seq() {
            return Ok(None);
        }
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let dense = self.at(mid)?;
            if dense.info.last_seq < seq {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        ensure!(lo < self.len(), "catalog missing sequence range");
        let dense = self.at(lo)?;
        ensure!(dense.info.first_seq <= seq, "catalog sequence gap");
        Ok(Some(dense))
    }

    /// Reads an exact checkpoint locator from the pinned root or trusted live delta.
    /// Error cases:
    /// - membership/nonmembership cannot be verified, or a locator has invalid framing
    pub fn checkpoint(&self, offset: Offset) -> anyhow::Result<Option<CheckpointLocator>> {
        let live = self.delta.iter().flat_map(|c| c.checkpoints.iter()).find(|c| c.offset == offset);
        let locator = match live {
            Some(locator) => Some(locator.clone()),
            None => self.read.read(&self.certified.root, &checkpoint_key(offset))?.map(|v| decode(&v)).transpose()?,
        };
        if let Some(locator) = &locator {
            validate_locator(locator, offset)?;
        }
        Ok(locator)
    }

    /// Adds one completion from trusted writer bytes or completed verified replay.
    /// Error cases:
    /// - delta is full, seqs are discontinuous, or canonical/framing checks fail
    pub(super) fn append(&self, info: SegmentInfo, checkpoints: Arc<[CheckpointLocator]>) -> anyhow::Result<Self> {
        ensure!(self.delta.len() < MAX_LIVE_SEGMENTS, "catalog live delta full");
        ensure!(info.first_seq == self.next_seq(), "catalog completion gap");
        let dense = dense_completion(self.len(), self.manifest_bytes(), self.prefix(), info, &checkpoints)?;
        let mut delta = self.delta.to_vec();
        delta.push(Completion { dense, checkpoints });
        Ok(Self {
            read: self.read.clone(),
            certified: self.certified.clone(),
            delta: delta.into(),
            lease: self.lease.clone(),
        })
    }

    /// Stages bounded updates and publishes a signed durable head before exposing the result.
    /// Error cases:
    /// - the owner, proof verification, immutable commit, or head publication fails
    pub fn fold(&self, owner: &mut CatalogOwner, boundary: Option<CheckpointLocator>) -> anyhow::Result<Self> {
        crate::log::catalog_commit::fold(self, owner, boundary.as_slice())
    }

    /// Folds an arbitrary active locator batch without exceeding the Store's per-update envelope.
    /// Error cases:
    /// - staging, exact old-value checks, durable commit, or publication fails
    pub(super) fn fold_locators(
        &self,
        owner: &mut CatalogOwner,
        locators: &[CheckpointLocator],
    ) -> anyhow::Result<Self> {
        crate::log::catalog_commit::fold(self, owner, locators)
    }

    /// Proves C's completed prefix using one dense point proof under this later root T.
    /// Error cases:
    /// - namespace/version/range regresses, or the signed prefix/boundary does not match
    pub fn prove_extension(&self, earlier: &CatalogRoot) -> anyhow::Result<()> {
        ensure!(
            self.certified.root.namespace == earlier.root.namespace
                && self.certified.root.version >= earlier.root.version,
            "catalog continuation identity/version"
        );
        ensure!(
            self.certified.completed_segments >= earlier.completed_segments,
            "catalog continuation count regressed"
        );
        if earlier.completed_segments == 0 {
            ensure!(
                earlier.prefix == [0; 32] && earlier.manifest_bytes == 0 && earlier.next_seq == 0,
                "empty catalog prefix mismatch"
            );
        } else {
            let dense = self.at(earlier.completed_segments - 1)?;
            ensure!(
                dense.prefix == earlier.prefix
                    && dense.manifest_offset + dense.manifest_line.len() as u64 == earlier.manifest_bytes
                    && dense.info.last_seq + 1 == earlier.next_seq,
                "catalog continuation prefix mismatch"
            );
        }
        if self.certified.completed_segments == earlier.completed_segments {
            ensure!(self.certified.meta() == earlier.meta(), "catalog continuation metadata mismatch");
        }
        Ok(())
    }

    /// Rebases trusted live deltas onto the same owner's later published/maintenance head.
    /// Error cases:
    /// - counts or prefix identity differ from the actual trusted delta
    pub(super) fn rebase(&self, base: Self) -> anyhow::Result<Self> {
        ensure!(
            self.certified.root.namespace == base.certified.root.namespace
                && base.certified.root.version >= self.certified.root.version,
            "catalog rebase namespace/version"
        );
        ensure!(
            base.certified.completed_segments >= self.certified.completed_segments
                && base.certified.completed_segments <= self.len(),
            "catalog rebase range"
        );
        let drop = usize::try_from(base.certified.completed_segments - self.certified.completed_segments)?;
        if drop == 0 {
            ensure!(base.certified.meta() == self.certified.meta(), "catalog maintenance changed metadata");
        } else {
            let dense = &self.delta[drop - 1].dense;
            ensure!(
                dense.prefix == base.certified.prefix
                    && dense.manifest_offset + dense.manifest_line.len() as u64 == base.certified.manifest_bytes
                    && dense.info.last_seq + 1 == base.certified.next_seq,
                "catalog folded delta mismatch"
            );
        }
        Ok(Self { delta: self.delta[drop..].into(), ..base })
    }

    /// Builds the explicit public listing in canonical ordinal order.
    /// Error cases:
    /// - any ordinal, line boundary, or adjacent sequence range fails verification
    pub fn to_jsonl(&self) -> anyhow::Result<Vec<u8>> {
        Ok(self.listing_after(None)?.bytes.to_vec())
    }

    /// Builds the listing as `to_jsonl` does, proving only the ordinals after `known` when `known`
    /// is a listing of this catalog's earlier prefix. Each ordinal commits to the prefix before it,
    /// and the result must end at this root's commitment, so a `known` from any other catalog or
    /// history fails that check and the listing is rebuilt in full.
    /// Error cases:
    /// - as `to_jsonl`
    pub fn listing_after(&self, known: Option<&CatalogListing>) -> anyhow::Result<CatalogListing> {
        let start = known.filter(|known| known.count <= self.len()).cloned().unwrap_or_default();
        if start.count == self.len()
            && start.bytes.len() as u64 == self.manifest_bytes()
            && start.prefix == self.prefix()
            && start.count > 0
        {
            // Nothing completed since: the same bytes, neither proved nor copied again.
            return Ok(start);
        }
        let extended = (|| {
            let mut bytes = start.bytes.to_vec();
            let (mut next_seq, mut prefix) = (start.next_seq, start.prefix);
            for ordinal in start.count..self.len() {
                let dense = self.at(ordinal)?;
                ensure!(
                    dense.manifest_offset == bytes.len() as u64 && dense.info.first_seq == next_seq,
                    "noncontiguous catalog listing"
                );
                ensure!(dense.previous_prefix == prefix, "catalog listing prefix gap");
                prefix = dense.prefix;
                next_seq = dense.info.last_seq + 1;
                bytes.extend(dense.manifest_line);
            }
            ensure!(
                bytes.len() as u64 == self.manifest_bytes() && prefix == self.prefix(),
                "catalog listing byte/prefix boundary mismatch"
            );
            Ok(CatalogListing { bytes: Arc::from(bytes), count: self.len(), next_seq, prefix })
        })();
        match extended {
            Err(_) if start.count > 0 => self.listing_after(None),
            result => result,
        }
    }
}

impl CatalogRoot {
    /// The metadata value authenticated under this root.
    pub(super) fn meta(&self) -> CatalogMeta {
        CatalogMeta {
            completed_segments: self.completed_segments,
            manifest_bytes: self.manifest_bytes,
            next_seq: self.next_seq,
            prefix: self.prefix,
        }
    }
}

/// Validates canonical bytes, bounded names, ranges, and integer arithmetic.
/// Error cases:
/// - a descriptor cannot have been emitted by this writer
pub(super) fn validate_dense(dense: &DenseSegmentInfo, ordinal: u64) -> anyhow::Result<()> {
    let info = &dense.info;
    ensure!(dense.ordinal == ordinal && info.file == segment_name(info.first_seq), "catalog descriptor identity");
    ensure!(info.first_seq <= info.last_seq && info.last_seq < u64::MAX && info.bytes > 0, "catalog descriptor range");
    ensure!(info.sha256.len() == 64 && hex::decode(&info.sha256)?.len() == 32, "catalog descriptor hash");
    ensure!(dense.prefix == extend_catalog_prefix(dense.previous_prefix, dense), "catalog dense prefix mismatch");
    ensure!(dense.manifest_line == listing_line(info)?, "noncanonical catalog manifest line");
    ensure!(dense.manifest_offset.checked_add(dense.manifest_line.len() as u64).is_some(), "catalog manifest overflow");
    Ok(())
}

/// Validates the exact checkpoint key, seq and physical line bounds.
/// Error cases:
/// - the locator is inconsistent or empty
pub(super) fn validate_locator(locator: &CheckpointLocator, offset: Offset) -> anyhow::Result<()> {
    ensure!(
        locator.offset == offset && offset.index > 0 && locator.checkpoint.seq == offset.seq,
        "checkpoint locator identity"
    );
    ensure!(
        locator.segment_first_seq <= offset.seq && locator.start > 0 && locator.end > locator.start,
        "checkpoint locator framing"
    );
    Ok(())
}

/// The stable dense ordinal key in the catalog namespace.
pub(super) fn ordinal_key(ordinal: u64) -> Vec<u8> {
    [b"log/catalog/v1/segment/".as_slice(), &ordinal.to_be_bytes()].concat()
}

/// The stable exact checkpoint key in the catalog namespace.
pub(super) fn checkpoint_key(offset: Offset) -> Vec<u8> {
    [b"log/catalog/v1/checkpoint/".as_slice(), &offset.seq.to_be_bytes(), &offset.index.to_be_bytes()].concat()
}

/// Decodes one bounded verified leaf.
/// Error cases:
/// - the leaf is oversized or malformed
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> anyhow::Result<T> {
    ensure!(bytes.len() <= MAX_LEAF_BYTES, "catalog leaf too large");
    serde_json::from_slice(bytes).context("catalog leaf")
}

/// Requires a verified membership leaf; missing ordinals never become successful absence.
/// Error cases:
/// - proof verification, required membership, or decoding fails
pub(super) fn required<T: DeserializeOwned>(read: &dyn CatalogRead, root: &Root, key: &[u8]) -> anyhow::Result<T> {
    decode(&read.read(root, key)?.context("missing required catalog leaf")?)
}
