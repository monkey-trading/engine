//! Compatibility accessors for full-audit manifests and lazy authenticated runtime views.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use crate::log::segments::{listing_line, segment_name};
use crate::log::{Manifest, Offset, SegmentInfo};

/// In-process provenance: only verified replay and the trusted writer can construct this marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadAuthority(pub(super) ());

/// Commitment captured from the actual bytes decoded at replay or supplied to the writer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivePrefix {
    pub info: SegmentInfo,
    pub last: Offset,
}

/// Why an answer the enclave vouches for was refused: it starts before what this node's start verified, which the
/// log's public archive holds as the host wrote it.
#[derive(Debug)]
pub(crate) struct NotVerified {
    /// The offset the answer would start at.
    pub start: Offset,
    /// The first offset this node's start verified (`Manifest::verified_from`).
    pub verified_from: Offset,
}

impl std::fmt::Display for NotVerified {
    /// Names the offset, the first one this node's start verified, and the archive.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            concat!(
                "offset {} is before offset {}, the first this node's start verified: ",
                "read it from the log's public archive"
            ),
            self.start, self.verified_from
        )
    }
}

impl std::error::Error for NotVerified {}

impl Manifest {
    /// Refuses unsigned host-manifest data as a source for backfill or public archive responses.
    /// Error cases:
    /// - a nonempty view has no verified replay/writer provenance
    pub(crate) fn ensure_verified(&self) -> anyhow::Result<()> {
        ensure!(
            self.authority.is_some()
                || (self.segments.is_empty()
                    && self.catalog.is_none()
                    && self.active.is_none()
                    && self.flushed.is_none()),
            "unsigned manifest is only a locator"
        );
        Ok(())
    }

    /// Refuses an answer the enclave vouches for, from `start` on, that would read entries this node's start did
    /// not verify (`verified_from`), as one from before the first segment on this host is refused.
    /// Error cases:
    /// - `start` is before `verified_from` (`NotVerified`, naming the archive)
    pub(crate) fn ensure_served(&self, start: Offset) -> Result<(), NotVerified> {
        match start < self.verified_from {
            true => Err(NotVerified { start, verified_from: self.verified_from }),
            false => Ok(()),
        }
    }

    /// Completed segment count, without hydrating historical descriptors.
    pub fn completed_len(&self) -> u64 {
        self.catalog.as_ref().map_or(self.segments.len() as u64, |c| c.len())
    }

    /// Reads one completed descriptor from either trusted memory or an authenticated leaf.
    /// Error cases:
    /// - a required descriptor or proof is missing or invalid
    pub fn completed_at(&self, ordinal: u64) -> anyhow::Result<SegmentInfo> {
        match &self.catalog {
            Some(catalog) => Ok(catalog.at(ordinal)?.info),
            None => Ok(self.segments.get(usize::try_from(ordinal)?).context("missing completed segment")?.clone()),
        }
    }

    /// First seq outside the completed catalog, with checked arithmetic for full-audit input.
    /// Error cases:
    /// - a full-audit descriptor's seq overflows
    pub fn unlisted_from(&self) -> anyhow::Result<u64> {
        match &self.catalog {
            Some(catalog) => Ok(catalog.next_seq()),
            None => self.segments.last().map_or(Ok(0), |s| s.last_seq.checked_add(1).context("segment seq overflow")),
        }
    }

    /// Finds the completed segment for seq using bounded ordinal probes.
    /// Error cases:
    /// - proof verification fails or a required certified range has a gap
    pub fn containing(&self, seq: u64) -> anyhow::Result<Option<(u64, SegmentInfo)>> {
        if let Some(catalog) = &self.catalog {
            return Ok(catalog.lookup_seq(seq)?.map(|dense| (dense.ordinal, dense.info)));
        }
        let ordinal = self.segments.partition_point(|s| s.last_seq < seq);
        let Some(info) = self.segments.get(ordinal) else {
            return Ok(None);
        };
        ensure!(info.first_seq <= seq, "completed segment sequence gap");
        Ok(Some((ordinal as u64, info.clone())))
    }

    /// Proves an exact completed file lookup, with absence derived only from the trusted catalog.
    /// Error cases:
    /// - a required range/proof is invalid
    pub fn lookup(&self, first_seq: u64) -> anyhow::Result<Option<SegmentInfo>> {
        Ok(self.containing(first_seq)?.map(|(_, info)| info).filter(|info| info.first_seq == first_seq))
    }

    /// Validates the writer's active prefix before a reader uses it.
    /// Error cases:
    /// - identity, byte count, seq, or flush frontier contradict the completed prefix
    pub(super) fn active_prefix(&self) -> anyhow::Result<Option<ActivePrefix>> {
        let Some(active) = self.active.clone() else {
            return Ok(None);
        };
        let info = &active.info;
        ensure!(
            info.first_seq == self.unlisted_from()? && info.file == segment_name(info.first_seq),
            "active prefix identity"
        );
        ensure!(
            info.bytes > 0 && info.first_seq <= info.last_seq && info.last_seq == active.last.seq,
            "active prefix range"
        );
        ensure!(self.flushed.is_some_and(|flushed| active.last <= flushed), "active prefix exceeds flushed frontier");
        Ok(Some(active))
    }

    /// Explicitly lists the canonical public manifest; normal startup never calls this method.
    /// Error cases:
    /// - a required catalog proof fails or a listing cannot be encoded
    pub fn canonical_jsonl(&self) -> anyhow::Result<Vec<u8>> {
        match &self.catalog {
            Some(catalog) => {
                self.ensure_verified()?;
                catalog.to_jsonl()
            }
            None => self.segments.iter().try_fold(Vec::new(), |mut out, info| {
                out.extend(listing_line(info)?);
                Ok(out)
            }),
        }
    }
}
