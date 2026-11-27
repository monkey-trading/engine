//! A backfill's reads of completed segments: from the bucket's ranges first, rebuilt to the bytes the segment's
//! listing names, then from each node origin in order, mirrors first. A node that no longer holds a segment
//! answers `308` to the bucket's object of the range holding its first seq, and a reader that names no bucket of its
//! own rebuilds it from the bucket that redirect names, reading on from there; one that names its own reads only
//! that, so a node origin cannot send it elsewhere. A segment is taken only as the bytes its listing names,
//! whichever source served it.

use std::collections::BTreeMap;

use anyhow::{Context, anyhow};
use reqwest::{Client, Url};
use tracing::{debug, info, warn};

use crate::archive::range_seqs;
use crate::archive::read::{Bucket, Ranges, Unread};
use crate::log::SegmentInfo;
use crate::standby::MAX_ARCHIVE_BYTES;
use crate::standby::archive::{Fetched, check_listing, fetch_from, fetch_manifest, validate_listing};

/// The most bytes of a segment rebuilt from the bucket, a node's bound on a segment it serves; a mebibyte in tests.
pub(crate) fn max_rebuilt_bytes() -> u64 {
    if cfg!(test) && !crate::archive::production_sizes() { 1 << 20 } else { MAX_ARCHIVE_BYTES }
}

/// Where a backfill reads the archive: node origins in order, mirrors first, and the bucket before them when the
/// reader's own `[archive]` names one. A list of node origins names no bucket.
pub(crate) trait ArchiveOrigins: Sync {
    /// The node origins, in the order they are asked.
    fn nodes(&self) -> &[String];

    /// The bucket the reader reads first, if any.
    fn bucket(&self) -> Option<&Bucket> {
        None
    }
}

/// The bucket a reader reads first, and the node origins after it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Origins {
    pub bucket: Option<Bucket>,
    pub nodes: Vec<String>,
}

/// A backfill's reads of completed segments from one set of origins, holding the bucket's last object.
pub(crate) struct Segments<'a> {
    http: &'a Client,
    nodes: &'a [String],
    /// The bucket's ranges: the reader's own, or those a node's redirect named.
    ranges: Option<Ranges>,
    /// Whether `ranges` is the bucket the reader's own `[archive]` names, which no redirect replaces.
    own: bool,
    /// The first range the bucket lacked or could not serve: the bucket holds a prefix of the log, so from that
    /// range on the nodes are asked first, and the bucket again only for a segment none of them serves.
    behind: Option<u64>,
    /// The objects reported missing, each once, with the range each holds, until a segment over it is served.
    reported: BTreeMap<Url, u64>,
}

impl ArchiveOrigins for [String] {
    fn nodes(&self) -> &[String] {
        self
    }
}

impl ArchiveOrigins for Vec<String> {
    fn nodes(&self) -> &[String] {
        self
    }
}

impl<const N: usize> ArchiveOrigins for [String; N] {
    fn nodes(&self) -> &[String] {
        self
    }
}

impl ArchiveOrigins for Origins {
    fn nodes(&self) -> &[String] {
        &self.nodes
    }

    fn bucket(&self) -> Option<&Bucket> {
        self.bucket.as_ref()
    }
}

impl std::fmt::Display for Origins {
    /// The bucket, then each node origin, as a backfill reads them.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bucket = self.bucket.iter().map(ToString::to_string);
        f.write_str(&bucket.chain(self.nodes.iter().cloned()).collect::<Vec<_>>().join(", "))
    }
}

impl<'a> Segments<'a> {
    /// Reads of `origins` over `http`.
    pub(crate) fn new(http: &'a Client, origins: &'a (impl ArchiveOrigins + ?Sized)) -> Self {
        let ranges = origins.bucket().cloned().map(Ranges::new);
        let own = ranges.is_some();
        Self { http, nodes: origins.nodes(), ranges, own, behind: None, reported: BTreeMap::new() }
    }

    /// The archive's manifest, from the node origins (`fetch_manifest`).
    /// Error cases:
    /// - see `fetch_manifest`
    pub(crate) async fn manifest(&self) -> anyhow::Result<Vec<SegmentInfo>> {
        fetch_manifest(self.http, self.nodes).await
    }

    /// The bytes of the completed segment `listed`, as its listing names them: from the bucket's ranges first,
    /// unless the bucket lacked or failed an earlier range, then from each node origin in order, following a
    /// node's redirect to the bucket, and from the bucket once more when no node serves it. A `404` from the
    /// bucket for a segment no node serves pages Warning `archive_segment_missing`, once per object, which a
    /// served segment over the object resolves.
    /// Error cases:
    /// - the listing is not canonical (`ArchiveIntegrity`)
    /// - no source serves the listed bytes: the last failure, an `ArchiveIntegrity` for bytes other than the
    ///   listing's
    pub(crate) async fn segment(&mut self, listed: &SegmentInfo) -> anyhow::Result<Vec<u8>> {
        let bytes = self.read(listed).await?;
        let ranges = listed.first_seq / range_seqs()..=listed.last_seq / range_seqs();
        self.reported.retain(|object, index| {
            let served = ranges.contains(index);
            if served {
                info!(alert_resolved = "archive_segment_missing", %object, "a segment over the object is served");
            }
            !served
        });
        Ok(bytes)
    }

    /// `segment`'s read of the listed bytes.
    async fn read(&mut self, listed: &SegmentInfo) -> anyhow::Result<Vec<u8>> {
        validate_listing(listed)?;
        let index = listed.first_seq / range_seqs();
        let (mut failed, mut missing) = (None, None);
        let ahead = self.behind.is_none_or(|behind| index < behind);
        if ahead && self.ranges.is_some() {
            match self.rebuild(listed, None).await {
                Ok(bytes) => return Ok(bytes),
                Err(unread) => self.note(unread, (&mut failed, &mut missing)),
            }
        }
        let mut redirected = false;
        for origin in self.nodes {
            match fetch_from(self.http, origin, &listed.file, MAX_ARCHIVE_BYTES).await {
                Ok(Fetched::Bytes(bytes)) => match check_listing(listed, &bytes) {
                    Ok(()) => return Ok(bytes),
                    Err(e) => failed = Some(e.context(format!("served by {origin}"))),
                },
                Ok(Fetched::Moved(location)) => match self.redirect(&location, listed.first_seq) {
                    Ok(bucket) => {
                        redirected = true;
                        match self.rebuild(listed, Some(bucket)).await {
                            Ok(bytes) => return Ok(bytes),
                            Err(unread) => self.note(unread, (&mut failed, &mut missing)),
                        }
                    }
                    Err(e) => failed = Some(e.context(format!("redirected by {origin}"))),
                },
                Err(e) => failed = Some(e),
            }
        }
        // A `404` for an earlier range may be a stale one, which a cache in front of the bucket answers.
        if !ahead && !redirected && self.ranges.is_some() {
            match self.rebuild(listed, None).await {
                Ok(bytes) => return Ok(bytes),
                Err(unread) => self.note(unread, (&mut failed, &mut missing)),
            }
        }
        if let Some((object, index)) = missing {
            self.report(listed, &object, index);
            let gone = format!("archive_segment_missing: the archive's bucket has no object at {object}");
            failed = Some(match failed {
                Some(failed) => failed.context(gone),
                None => anyhow!(gone),
            });
        }
        Err(failed.unwrap_or_else(|| anyhow!("no archive origin to read")))
            .with_context(|| format!("every archive origin failed for {}", listed.file))
    }

    /// The bucket a node's redirect to `location` for the segment starting at `first_seq` names, which must be the
    /// reader's own when it names one.
    /// Error cases:
    /// - see `Bucket::redirected`, or the redirect names another bucket than the reader's own
    fn redirect(&self, location: &Url, first_seq: u64) -> anyhow::Result<Bucket> {
        let bucket = Bucket::redirected(location, first_seq)?;
        match self.ranges.as_ref().filter(|_| self.own) {
            Some(own) if own.bucket() != &bucket => {
                Err(anyhow!("a redirect to {location} leaves the bucket [archive] names, {}", own.bucket()))
            }
            _ => Ok(bucket),
        }
    }

    /// `listed`'s bytes rebuilt from the ranges of `redirect`'s bucket, which the reader holds from then on, or of
    /// the bucket it holds, checked against the listing. Bytes other than the listing's are no integrity failure
    /// of what a node serves: the uploader takes a range a copy spells otherwise as the log's, so the segment is
    /// taken from a node instead.
    /// Error cases:
    /// - see `Ranges::seqs`, and bytes other than the listing's (`Refused`), never an `ArchiveIntegrity`
    async fn rebuild(&mut self, listed: &SegmentInfo, redirect: Option<Bucket>) -> Result<Vec<u8>, Unread> {
        if let Some(bucket) = redirect.filter(|bucket| self.ranges.as_ref().is_none_or(|held| held.bucket() != bucket))
        {
            (self.ranges, self.behind) = (Some(Ranges::new(bucket)), None);
        }
        let ranges = self.ranges.as_mut().expect("a bucket to read: the caller holds or names one");
        let bucket = ranges.bucket().to_string();
        // A listing names no more bytes than a node serves a segment, which bounds what the rebuild holds.
        let limit = listed.bytes.min(max_rebuilt_bytes());
        let bytes = ranges.seqs(self.http, (listed.first_seq, listed.last_seq), limit).await?;
        check_listing(listed, &bytes).map_err(|e| Unread::Refused(anyhow!("rebuilt from {bucket}: {e}")))?;
        debug!(segment = %listed.file, %bucket, "took the segment from the archive's bucket");
        Ok(bytes)
    }

    /// Records why the bucket did not serve a segment: a missing object and an unreachable bucket put it behind,
    /// and a refused object is logged, since the uploader pages for an object other than the log's.
    fn note(&mut self, unread: Unread, (failed, missing): (&mut Option<anyhow::Error>, &mut Option<(Url, u64)>)) {
        match unread {
            Unread::Missing { index, object } => {
                self.behind = Some(self.behind.map_or(index, |behind| behind.min(index)));
                debug!(%object, "the archive's bucket has no object here yet");
                *missing = Some((object, index));
            }
            Unread::Unreachable(e) => {
                let at = self.ranges.as_ref().map(|ranges| ranges.bucket().to_string()).unwrap_or_default();
                warn!("the archive's bucket at {at} could not be read, so the nodes are asked first: {e:#}");
                self.behind = Some(0);
                *failed = Some(e);
            }
            Unread::Refused(e) => {
                warn!("the archive's bucket served an object that holds other bytes than the segment's listing: {e:#}");
                *failed = Some(e);
            }
        }
    }

    /// Pages Warning `archive_segment_missing` for `object`, which `listed` needs and no node served, once.
    fn report(&mut self, listed: &SegmentInfo, object: &Url, index: u64) {
        if self.reported.insert(object.clone(), index).is_none() {
            warn!(
                alert = "archive_segment_missing",
                %object,
                "the archive's bucket answers 404 for the object holding seqs of {} ({} to {}), which no archive \
                 origin served either: the log is not complete without it, so the read stops here",
                listed.file,
                listed.first_seq,
                listed.last_seq
            );
        }
    }
}

#[cfg(test)]
mod tests;
