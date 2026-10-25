//! Lazy authenticated backfill across completed segments and committed active prefixes.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use tokio::sync::watch;

use crate::log::checked::{self, CheckedChunks, ChunkHashes, Progress};
use crate::log::{LogEntry, Manifest, Offset, SegmentInfo, offset_of, parse_line};

/// Entries at or after an inclusive offset, bounded by the writer's flush frontier.
pub struct LogReader {
    dir: PathBuf,
    from: Offset,
    manifest: watch::Receiver<Manifest>,
    segment: Option<OpenSegment>,
    last: Option<Offset>,
    held: Option<(Offset, LogEntry)>,
}

struct OpenSegment {
    ordinal: u64,
    first_seq: u64,
    consumed: u64,
    lines: Option<Box<dyn BufRead + Send>>,
    limit: u64,
    complete: bool,
    started: bool,
    expected_last: Option<Offset>,
    /// The last descriptor checked (and whether it was complete), its chunk hashes, and the hash
    /// state that lets a longer prefix of the same file be checked from where that check ended.
    verified: Option<(SegmentInfo, bool, ChunkHashes, Progress)>,
}

impl LogReader {
    /// Resolves the starting segment from authenticated point probes, without directory listing.
    /// Error cases:
    /// - a required catalog proof or starting descriptor is unavailable or invalid
    pub fn open(dir: &Path, from: Offset, manifest: watch::Receiver<Manifest>) -> anyhow::Result<Self> {
        let mut reader = Self { dir: dir.to_owned(), from, manifest, segment: None, last: None, held: None };
        reader.open_start()?;
        Ok(reader)
    }

    /// Releases the open file and chunk buffer; the next call checks each chunk it rereads against
    /// the hashes kept from the segment's last check.
    pub fn release(&mut self) {
        if let Some(segment) = &mut self.segment {
            segment.lines = None;
        }
    }

    /// Returns the next authenticated entry, or None once the published flush frontier is reached.
    /// Error cases:
    /// - disk bytes, required proofs, framing, or contiguous command seqs fail verification
    pub fn next_entry(&mut self) -> anyhow::Result<Option<(Offset, LogEntry)>> {
        let next = match self.held.take() {
            Some(held) => held,
            None => match self.read_next()? {
                Some(next) => next,
                None => return Ok(None),
            },
        };
        if self.manifest.borrow().flushed.is_some_and(|flushed| next.0 <= flushed) {
            return Ok(Some(next));
        }
        self.held = Some(next);
        Ok(None)
    }

    /// Decodes bytes exclusively from checked private chunk buffers. The published view is cloned
    /// only to open a segment or to handle its end; a legacy view lists every completed segment.
    /// Error cases:
    /// - catalog membership, exact bytes, framing, or offset continuity is invalid
    fn read_next(&mut self) -> anyhow::Result<Option<(Offset, LogEntry)>> {
        loop {
            if self.segment.is_none() {
                self.open_start()?;
                if self.segment.is_none() {
                    return Ok(None);
                }
            }
            self.manifest.borrow().ensure_verified()?;
            let segment = self.segment.as_mut().expect("segment opened above");
            if segment.lines.is_none() {
                let view = self.manifest.borrow().clone();
                let Some((info, complete, expected_last)) = descriptor(&view, segment)? else {
                    return Ok(None);
                };
                ensure!(segment.consumed <= info.bytes, "flushed prefix shrank");
                let path = self.dir.join(&info.file);
                let hashes = match segment.verified.take() {
                    Some((known, was_complete, hashes, progress)) if known == info && was_complete == complete => {
                        segment.verified = Some((known, was_complete, hashes.clone(), progress));
                        hashes
                    }
                    known => {
                        let progress =
                            known.and_then(|(known, _, _, progress)| (known.file == info.file).then_some(progress));
                        let (hashes, progress) = checked::extend(&path, &info, complete, progress)?;
                        segment.verified = Some((info.clone(), complete, hashes.clone(), progress));
                        hashes
                    }
                };
                segment.lines = Some(Box::new(BufReader::new(CheckedChunks::open(&path, hashes, segment.consumed)?)));
                segment.limit = info.bytes;
                segment.complete = complete;
                segment.expected_last = expected_last;
            }
            let mut line = Vec::new();
            let n = segment.lines.as_mut().expect("checked reader opened above").read_until(b'\n', &mut line)?;
            if n > 0 {
                ensure!(line.ends_with(b"\n"), "committed log prefix ends mid-line");
                segment.consumed = segment.consumed.checked_add(n as u64).context("segment byte overflow")?;
                ensure!(segment.consumed <= segment.limit, "reader exceeded committed prefix");
                let Some(entry) = parse_line(0, &line)? else {
                    continue;
                };
                if !segment.started {
                    ensure!(
                        matches!(&entry, LogEntry::Command(c) if c.sequenced.seq == segment.first_seq),
                        "segment does not start with its named command"
                    );
                    segment.started = true;
                }
                if let (LogEntry::Command(c), Some(prev)) = (&entry, self.last) {
                    ensure!(
                        prev.seq.checked_add(1) == Some(c.sequenced.seq),
                        "seq {} follows seq {}",
                        c.sequenced.seq,
                        prev.seq
                    );
                }
                if !matches!(&entry, LogEntry::Command(_)) {
                    ensure!(self.last.is_some_and(|last| last.index < u32::MAX), "log offset index overflow");
                }
                let offset = offset_of(self.last, &entry)?;
                if let LogEntry::Checkpoint(c) = &entry {
                    ensure!(c.seq == offset.seq, "checkpoint seq does not match framing");
                }
                self.last = Some(offset);
                if offset >= self.from {
                    return Ok(Some((offset, entry)));
                }
                continue;
            }
            ensure!(segment.consumed == segment.limit, "truncated committed prefix");
            let view = self.manifest.borrow().clone();
            if let Some(expected) = segment.expected_last {
                ensure!(self.last == Some(expected), "active prefix offset mismatch");
            }
            let ordinal = segment.ordinal;
            // A rotation can complete this segment while its old active-prefix reader is open.
            if ordinal < view.completed_len() {
                let completed = view.completed_at(ordinal)?;
                ensure!(completed.first_seq == segment.first_seq, "catalog segment identity changed");
                if completed.bytes > segment.consumed || !segment.complete {
                    segment.lines = None;
                    continue;
                }
                ensure!(
                    self.last.is_some_and(|last| last.seq == completed.last_seq),
                    "completed segment last seq mismatch"
                );
                let next = ordinal.checked_add(1).context("catalog ordinal overflow")?;
                if next < view.completed_len() {
                    let info = view.completed_at(next)?;
                    self.segment = Some(OpenSegment::new(next, info.first_seq));
                    continue;
                }
                if let Some(active) = view.active_prefix()? {
                    self.segment = Some(OpenSegment::new(next, active.info.first_seq));
                    continue;
                }
                // Retain the completed ordinal so a later call can advance after publication.
                return Ok(None);
            }
            if let Some(active) = view.active_prefix()? {
                ensure!(
                    active.info.first_seq == segment.first_seq && active.info.bytes >= segment.consumed,
                    "active prefix replaced"
                );
                if active.info.bytes > segment.consumed {
                    segment.lines = None;
                    continue;
                }
            }
            return Ok(None);
        }
    }

    /// Resolves only the requested completed range or the writer's exact active descriptor.
    /// Error cases:
    /// - membership, required range, or active identity fails verification
    fn open_start(&mut self) -> anyhow::Result<()> {
        let view = self.manifest.borrow().clone();
        view.ensure_verified()?;
        if let Some((ordinal, info)) = view.containing(self.from.seq)? {
            self.segment = Some(OpenSegment::new(ordinal, info.first_seq));
        } else if let Some(active) = view.active_prefix()? {
            ensure!(self.from.seq >= active.info.first_seq, "no segment holds requested seq");
            self.segment = Some(OpenSegment::new(view.completed_len(), active.info.first_seq));
        }
        Ok(())
    }
}

impl OpenSegment {
    /// Begins a segment whose identity came from a trusted view.
    fn new(ordinal: u64, first_seq: u64) -> Self {
        Self {
            ordinal,
            first_seq,
            consumed: 0,
            lines: None,
            limit: 0,
            complete: false,
            started: false,
            expected_last: None,
            verified: None,
        }
    }
}

/// Resolves a descriptor from the pinned view, with no filesystem discovery or unsigned fallback.
/// Error cases:
/// - a required catalog leaf or active descriptor is invalid
fn descriptor(view: &Manifest, segment: &OpenSegment) -> anyhow::Result<Option<(SegmentInfo, bool, Option<Offset>)>> {
    if segment.ordinal < view.completed_len() {
        let info = view.completed_at(segment.ordinal)?;
        ensure!(info.first_seq == segment.first_seq, "catalog segment changed");
        Ok(Some((info, true, None)))
    } else {
        ensure!(segment.ordinal == view.completed_len(), "catalog ordinal gap");
        view.active_prefix()?
            .map(|active| {
                ensure!(active.info.first_seq == segment.first_seq, "active segment changed");
                Ok((active.info, false, Some(active.last)))
            })
            .transpose()
    }
}
