//! Direct physical suffix recovery from a certificate-selected checkpoint.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use crate::log::catalog::{CatalogRoot, CheckpointLocator, Completion, LazyCatalogView};
use crate::log::catalog_access::CatalogRead;
use crate::log::catalog_head::{CatalogDomain, VerifiedCatalogHead};
use crate::log::catalog_owner::CatalogOwner;
use crate::log::catalog_prefix::dense_completion;
use crate::log::checked::{self, CheckedChunks};
use crate::log::segments::{Segment, command_seq, listing_line, segment_name};
use crate::log::{
    ActivePrefix, LogEntry, MANIFEST, Manifest, Offset, ReadAuthority, SegmentInfo, SegmentedLog, offset_of, parse_line,
};

const MAX_MANIFEST_LINE_BYTES: u64 = 4096;

/// Certificate-selected current-segment prefix; completed count excludes this segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogBoundary {
    pub domain: CatalogDomain,
    pub catalog: CatalogRoot,
    pub checkpoint: CheckpointLocator,
    pub prefix: ActivePrefix,
    /// Certified physical seek; catalog.manifest_bytes counts the canonical public listing.
    pub manifest_seek: u64,
}

impl SegmentedLog {
    /// Replays every physical entry after the exact checkpoint, never filtering by claimed seq.
    /// Unreferenced files are ignored here; strict full audit retains directory-wide diagnosis.
    /// Error cases:
    /// - catalog proofs, checkpoint bytes/framing, prefix hash, or suffix bytes are invalid
    /// - a suffix descriptor is discontinuous or replay rejects any physical entry
    pub fn open_suffix(
        dir: &Path,
        read: Arc<dyn CatalogRead>,
        boundary: &CatalogBoundary,
        continuation: Option<&VerifiedCatalogHead>,
        mut visit: impl FnMut(LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let earlier = LazyCatalogView::open(read.clone(), boundary.catalog.clone())?;
        ensure!(
            earlier.checkpoint(boundary.checkpoint.offset)?.as_ref() == Some(&boundary.checkpoint),
            "exact checkpoint locator proof mismatch"
        );
        let mut catalog = match continuation {
            Some(head) => {
                ensure!(head.domain() == &boundary.domain, "catalog continuation deployment mismatch");
                let tip = LazyCatalogView::open(read.clone(), head.catalog().clone())?;
                tip.prove_extension(&boundary.catalog)?;
                ensure!(
                    tip.checkpoint(boundary.checkpoint.offset)?.as_ref() == Some(&boundary.checkpoint),
                    "continuation checkpoint locator mismatch"
                );
                tip
            }
            None => earlier.clone(),
        };
        // Versions after the signed tip were never acknowledged; recovery reads only at signed roots,
        // never a SQL latest root, and activation discards those versions after preflight.
        let tip_count = catalog.certified().completed_segments;
        if let Some(domain) = boundary.checkpoint.checkpoint.domain {
            ensure!(
                domain.chain_id == boundary.domain.chain_id && domain.vault.0 == boundary.domain.vault,
                "checkpoint/catalog deployment mismatch"
            );
        }
        let prefix = &boundary.prefix;
        ensure!(
            prefix.info.file == segment_name(prefix.info.first_seq)
                && prefix.info.first_seq == boundary.catalog.next_seq,
            "boundary segment identity"
        );
        ensure!(
            prefix.last == boundary.checkpoint.offset
                && prefix.info.last_seq == prefix.last.seq
                && prefix.info.bytes == boundary.checkpoint.end,
            "boundary prefix framing"
        );
        ensure!(boundary.checkpoint.segment_first_seq == prefix.info.first_seq, "boundary locator segment");
        let path = dir.join(MANIFEST);
        let mut manifest = match File::open(&path) {
            Ok(mut file) => {
                ensure!(file.metadata()?.len() >= boundary.manifest_seek, "manifest shorter than certified boundary");
                file.seek(SeekFrom::Start(boundary.manifest_seek))?;
                Some(BufReader::new(file))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && boundary.manifest_seek == 0 => None,
            Err(e) => return Err(e).context("opening suffix manifest locator"),
        };
        let mut manifest_len = boundary.manifest_seek;
        let mut canonical_offset = boundary.catalog.manifest_bytes;
        let mut pending_catalog = Vec::new();
        let mut ordinal = boundary.catalog.completed_segments;
        let mut previous_prefix = boundary.catalog.prefix;
        let mut last = None;
        let mut boundary_pending = true;
        let mut expected_first = prefix.info.first_seq;
        let mut line = Vec::new();
        while let Some(input) = manifest.as_mut() {
            line.clear();
            let n = (&mut *input).take(MAX_MANIFEST_LINE_BYTES + 1).read_until(b'\n', &mut line)?;
            ensure!(n as u64 <= MAX_MANIFEST_LINE_BYTES, "oversized suffix manifest locator");
            if n == 0 || !line.ends_with(b"\n") {
                break;
            }
            let listed: SegmentInfo = serde_json::from_slice(&line).context("suffix manifest locator")?;
            ensure!(line == listing_line(&listed)?, "noncanonical suffix manifest locator");
            ensure!(
                listed.file == segment_name(expected_first) && listed.first_seq == expected_first,
                "suffix manifest locator gap"
            );
            let known = if ordinal < tip_count { Some(catalog.at(ordinal)?) } else { None };
            if let Some(known) = &known {
                ensure!(
                    known.info == listed
                        && known.manifest_line == line
                        && known.manifest_offset == canonical_offset
                        && known.previous_prefix == previous_prefix,
                    "continuation physical descriptor mismatch"
                );
            }
            let segment = recover_segment(
                dir,
                expected_first,
                Some(&listed),
                boundary_pending.then_some(boundary),
                &mut last,
                &mut visit,
            )?;
            boundary_pending = false;
            ensure!(segment.info() == listed, "suffix segment descriptor mismatch");
            expected_first = listed.last_seq.checked_add(1).context("suffix seq overflow")?;
            let dense =
                dense_completion(ordinal, canonical_offset, previous_prefix, listed.clone(), &segment.checkpoints)?;
            previous_prefix = dense.prefix;
            if let Some(known) = known {
                ensure!(known == dense, "continuation descriptor/locator completeness mismatch");
                for locator in &segment.checkpoints {
                    ensure!(
                        catalog.checkpoint(locator.offset)?.as_ref() == Some(locator),
                        "continuation physical locator mismatch"
                    );
                }
            } else if catalog.live_full() || !pending_catalog.is_empty() {
                ensure!(ordinal == catalog.len() + pending_catalog.len() as u64, "pending suffix ordinal gap");
                pending_catalog.push(Completion { dense, checkpoints: segment.checkpoints.into() });
            } else {
                ensure!(ordinal == catalog.len(), "new suffix ordinal gap");
                catalog = catalog.append(listed, segment.checkpoints.into())?;
                ensure!(catalog.prefix() == previous_prefix, "new suffix prefix mismatch");
            }
            ordinal = ordinal.checked_add(1).context("suffix ordinal overflow")?;
            manifest_len = manifest_len.checked_add(line.len() as u64).context("manifest byte overflow")?;
            canonical_offset = canonical_offset.checked_add(line.len() as u64).context("canonical listing overflow")?;
            if ordinal == tip_count {
                ensure!(
                    previous_prefix == catalog.certified().prefix
                        && canonical_offset == catalog.certified().manifest_bytes
                        && expected_first == catalog.certified().next_seq,
                    "physical continuation tip boundary mismatch"
                );
            }
        }
        ensure!(ordinal >= tip_count, "manifest omits a certified continuation segment");
        let active_path = dir.join(segment_name(expected_first));
        let open = if boundary_pending || active_path.try_exists()? {
            Some(recover_segment(
                dir,
                expected_first,
                None,
                boundary_pending.then_some(boundary),
                &mut last,
                &mut visit,
            )?)
        } else {
            None
        };
        if let Some(segment) = &open {
            for locator in &segment.checkpoints {
                if let Some(known) = catalog.checkpoint(locator.offset)? {
                    ensure!(known == *locator, "active checkpoint differs from indexed continuation");
                }
            }
        }
        let active =
            open.as_ref().filter(|s| s.bytes > 0).and_then(|s| last.map(|last| ActivePrefix { info: s.info(), last }));
        ensure!(last.is_some_and(|last| last >= boundary.checkpoint.offset), "boundary was not reached");
        // No consumer may use an incomplete candidate view before authorized publication.
        let authority = pending_catalog.is_empty().then_some(ReadAuthority(()));
        let manifest =
            Manifest { segments: Arc::default(), catalog: Some(Arc::new(catalog)), active, authority, flushed: last };
        Ok(Self {
            dir: dir.to_owned(),
            last,
            manifest,
            manifest_len,
            open,
            pending_catalog,
            catalog_owner: None,
            catalog_domain: Some(boundary.domain.clone()),
        })
    }

    /// Exposes replay commitments; a large suffix remains unavailable until into_writer prepares it.
    pub fn manifest_view(&self) -> Manifest {
        self.manifest.clone()
    }

    /// Attaches current signed-tip writable ownership only after the caller completes #24 preflight.
    /// Error cases:
    /// - domain or current-head/live-prefix identity differs from the replayed candidate
    pub fn bind_catalog_owner(mut self, mut owner: CatalogOwner) -> anyhow::Result<Self> {
        ensure!(self.catalog_domain.as_ref() == Some(owner.domain()), "catalog owner deployment mismatch");
        let view = self.manifest.catalog.as_ref().context("recovery catalog missing")?;
        self.manifest.catalog = Some(Arc::new(owner.align(view)?));
        self.catalog_owner = Some(owner);
        Ok(self)
    }

    /// Finishes uncovered suffix catalog work only when into_writer is authorized after preflight.
    /// Error cases:
    /// - a durable fold or a trusted replay descriptor consistency check fails
    pub(super) fn prepare_catalog(&mut self) -> anyhow::Result<()> {
        if self.pending_catalog.is_empty() {
            return Ok(());
        }
        let final_listing = self.pending_catalog.last().expect("nonempty pending catalog");
        let expected_canonical_end = final_listing
            .dense
            .manifest_offset
            .checked_add(final_listing.dense.manifest_line.len() as u64)
            .context("canonical manifest length overflow")?;
        let owner = self.catalog_owner.as_mut().context("authorized catalog owner required")?;
        let mut catalog = owner.align(self.manifest.catalog.as_ref().context("suffix catalog missing")?)?;
        for completion in &self.pending_catalog {
            if catalog.live_full() {
                catalog = catalog.fold(owner, None)?;
            }
            ensure!(
                completion.dense.ordinal == catalog.len()
                    && completion.dense.manifest_offset == catalog.manifest_bytes(),
                "pending suffix catalog boundary"
            );
            catalog = catalog.append(completion.dense.info.clone(), completion.checkpoints.clone())?;
            ensure!(catalog.prefix() == completion.dense.prefix, "prepared suffix prefix mismatch");
        }
        ensure!(catalog.manifest_bytes() == expected_canonical_end, "prepared suffix manifest boundary");
        self.manifest.catalog = Some(Arc::new(catalog));
        self.manifest.authority = Some(ReadAuthority(()));
        self.pending_catalog.clear();
        Ok(())
    }

    /// Builds the initial durable catalog after read-only startup preflight has authorized writes.
    /// Error cases:
    /// - a completed-segment reread differs, proof storage fails, or the namespace is not empty
    pub fn attach_catalog(mut self, mut owner: CatalogOwner) -> anyhow::Result<Self> {
        ensure!(self.manifest.catalog.is_none() && self.pending_catalog.is_empty(), "catalog already attached");
        let mut catalog = owner.view();
        ensure!(catalog.is_empty(), "migration catalog is not fresh");
        let mut last = None;
        for info in self.manifest.segments.iter() {
            if catalog.live_full() {
                catalog = catalog.fold(&mut owner, None)?;
            }
            let segment = recover_segment(&self.dir, info.first_seq, Some(info), None, &mut last, &mut |_| Ok(()))?;
            catalog = catalog.append(info.clone(), segment.checkpoints.into())?;
        }
        catalog = catalog.fold(&mut owner, None)?;
        // Active locators were captured during the same replay that supplied the active commitment.
        if let Some(open) = &self.open {
            catalog = catalog.fold_locators(&mut owner, &open.checkpoints)?;
        }
        ensure!(catalog.len() == self.manifest.segments.len() as u64, "migration catalog omitted a completed segment");
        self.catalog_domain = Some(owner.domain().clone());
        self.catalog_owner = Some(owner);
        self.manifest.catalog = Some(Arc::new(catalog));
        self.manifest.segments = Arc::default();
        Ok(self)
    }
}

/// Decodes one physical segment and hashes exactly those bytes; the boundary prefix is withheld.
/// Error cases:
/// - IO, exact prefix framing/hash, completed descriptors, or replay validation fails
fn recover_segment(
    dir: &Path,
    first_seq: u64,
    listed: Option<&SegmentInfo>,
    boundary: Option<&CatalogBoundary>,
    last: &mut Option<Offset>,
    visit: &mut impl FnMut(LogEntry) -> anyhow::Result<()>,
) -> anyhow::Result<Segment> {
    let path = dir.join(segment_name(first_seq));
    let mut input: Box<dyn BufRead> = match listed {
        Some(info) => Box::new(BufReader::new(CheckedChunks::open(&path, checked::check(&path, info)?, 0)?)),
        None => Box::new(BufReader::new(File::open(&path)?)),
    };
    let mut segment = Segment::empty(first_seq);
    let mut boundary_seen = boundary.is_none();
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        ensure!(line.ends_with(b"\n"), "suffix segment ends mid-line");
        let start = segment.bytes;
        let entry = parse_line(0, &line)?;
        if let Some(entry) = &entry {
            if segment.commands == 0 {
                ensure!(command_seq(entry) == Some(first_seq), "suffix segment first command mismatch");
            }
            if !matches!(entry, LogEntry::Command(_)) {
                ensure!(last.is_some_and(|last| last.index < u32::MAX), "suffix offset overflow");
            }
            *last = Some(offset_of(*last, entry)?);
            if let LogEntry::Checkpoint(checkpoint) = entry {
                segment.checkpoints.push(CheckpointLocator {
                    segment_first_seq: first_seq,
                    offset: (*last).context("checkpoint before command")?,
                    start,
                    end: start + line.len() as u64,
                    checkpoint: checkpoint.clone(),
                });
            }
        }
        segment.record(&line, entry.as_ref());
        if !boundary_seen {
            let boundary = boundary.expect("a withheld prefix has a boundary");
            ensure!(segment.bytes <= boundary.checkpoint.end, "checkpoint boundary is not a physical line end");
            if segment.bytes == boundary.checkpoint.end {
                ensure!(
                    start == boundary.checkpoint.start && *last == Some(boundary.checkpoint.offset),
                    "checkpoint offset/framing mismatch"
                );
                ensure!(
                    matches!(&entry, Some(LogEntry::Checkpoint(c)) if c == &boundary.checkpoint.checkpoint),
                    "checkpoint entry mismatch"
                );
                ensure!(segment.info() == boundary.prefix.info, "boundary prefix bytes mismatch");
                boundary_seen = true;
            }
            continue;
        }
        // No claimed-seq comparison may discard a physical entry after the boundary.
        if let Some(entry) = entry {
            visit(entry)?;
        }
    }
    ensure!(boundary_seen, "checkpoint boundary missing");
    if let Some(info) = listed {
        ensure!(segment.info() == *info, "completed suffix descriptor mismatch");
    }
    if segment.commands == 0 {
        ensure!(last.map_or(0, |last| last.seq.saturating_add(1)) == first_seq, "empty suffix segment name");
    }
    Ok(segment)
}
