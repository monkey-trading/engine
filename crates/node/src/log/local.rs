//! The segments of a log that are on this host, each paired with its manifest listing by name. The oldest
//! listed segments may be absent, kept only in the public archive: every listed segment from the first one
//! here on must be here, in order, and only the last file may be unlisted, the segment still being written.
//! A read from the log's start refuses a log whose oldest segments are absent, and a read from an offset
//! refuses one the segments here do not reach back to, each naming the archive.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail, ensure};

use crate::log::segments::{Segment, list_segments};
use crate::log::{ActivePrefix, LogEntry, MANIFEST, Manifest, Offset, ReadAuthority, SegmentInfo, SegmentedLog};

/// A segment file on this host: the seq it is named for, its path, and its listing, when the manifest has one.
pub(super) type LocalSegment = (u64, PathBuf, Option<SegmentInfo>);

/// Why a read reached for a seq whose segment is not on this host: it is in the public archive.
#[derive(Debug)]
pub(crate) struct NotLocal {
    /// The seq the read asked for.
    pub seq: u64,
    /// The first seq the segments on this host hold, when the read knows it.
    pub first_local: Option<u64>,
}

impl std::fmt::Display for NotLocal {
    /// Names the seq, the first one held here, and the archive.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.first_local {
            Some(first) => write!(
                f,
                "seq {} is before seq {first}, the first this host's log holds: read it from the log's public archive",
                self.seq
            ),
            None => write!(
                f,
                "the segment holding seq {} is not on this host: read it from the log's public archive",
                self.seq
            ),
        }
    }
}

impl std::error::Error for NotLocal {}

/// Pairs `files`, the segment files on this host by the seq each is named for, with their listings in
/// `manifest` by name. The listings before the first file's are of segments only the archive holds;
/// `writing` drops listings past the files, which a writer rotating while the files were listed added.
/// Error cases:
/// - a listing from the first file's on is of a segment not here, or out of order
/// - a file other than the last is unlisted, or listings are left past the files while not `writing`
pub(super) fn pair(
    dir: &Path,
    manifest: &mut Manifest,
    files: Vec<(u64, PathBuf)>,
    writing: bool,
) -> anyhow::Result<Vec<LocalSegment>> {
    let first = files.first().map(|(first_seq, _)| *first_seq);
    let start =
        first.map_or(manifest.segments.len(), |first| manifest.segments.partition_point(|l| l.first_seq < first));
    let count = files.len();
    let mut paired = Vec::with_capacity(count);
    for (at, (first_seq, path)) in files.into_iter().enumerate() {
        let listed = manifest.segments.get(start + at).cloned();
        match &listed {
            Some(listed) => ensure!(
                listed.first_seq == first_seq,
                "{MANIFEST} lists {} where {} is in {}: a listed segment after the first on this host is missing",
                listed.file,
                path.display(),
                dir.display()
            ),
            None if at + 1 < count => {
                bail!("{} is followed by another segment but is not in {MANIFEST}", path.display())
            }
            None => {}
        }
        paired.push((first_seq, path, listed));
    }
    let listed_here = start + paired.iter().filter(|(_, _, listed)| listed.is_some()).count();
    if listed_here < manifest.segments.len() {
        ensure!(
            writing,
            "{MANIFEST} lists {} segments past the last of the {count} in {}",
            manifest.segments.len() - listed_here,
            dir.display()
        );
        Arc::make_mut(&mut manifest.segments).truncate(listed_here);
    }
    Ok(paired)
}

impl SegmentedLog {
    /// As `open`, over segment files listed before the manifest is read, handing `visit` each entry with
    /// the index in `files` of the segment it is in; `writing` drops manifest entries beyond those files.
    /// With `from`, the segments before the one holding its command are not read, and `visit` takes the
    /// entries from `from` on; without it, every segment is read, and the log's oldest may not be absent.
    /// Error cases:
    /// - as `open`; the files do not pair with the listings (`pair`)
    /// - the log's oldest listed segments are absent without `from`, or the segments here start after `from`
    ///   (`NotLocal`)
    pub(super) fn open_listed(
        dir: &Path,
        files: Vec<(u64, PathBuf)>,
        writing: bool,
        from: Option<Offset>,
        mut visit: impl FnMut(usize, LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let (mut manifest, manifest_len) = Manifest::read(dir)?;
        let files = pair(dir, &mut manifest, files, writing)?;
        let first = first_read(&manifest, &files, from.map(|from| from.seq))?;
        let (mut last, mut open, mut cursor) = (None, None, None);
        for (i, (first_seq, path, listed)) in files.iter().enumerate().skip(first) {
            let mut visit = |entry: LogEntry| {
                let at = crate::log::offset_of(cursor, &entry)?;
                cursor = Some(at);
                match from {
                    Some(from) if at < from => Ok(()),
                    _ => visit(i, entry),
                }
            };
            let segment = Segment::read(*first_seq, path, listed.as_ref(), &mut last, &mut visit)
                .with_context(|| format!("segment {}", path.display()))?;
            match listed {
                Some(listed) => ensure!(
                    segment.info() == *listed,
                    "{} does not match {MANIFEST}: on disk {:?}, listed {listed:?}",
                    path.display(),
                    segment.info()
                ),
                None => open = Some(segment),
            }
        }
        Segment::check_open(open.as_ref(), last)?;
        manifest.authority = Some(ReadAuthority(()));
        manifest.flushed = last;
        manifest.active =
            open.as_ref().filter(|s| s.bytes > 0).and_then(|s| last.map(|last| ActivePrefix { info: s.info(), last }));
        Ok(Self {
            dir: dir.to_owned(),
            last,
            manifest,
            manifest_len,
            open,
            pending_catalog: Vec::new(),
            catalog_owner: None,
            catalog_domain: None,
            catalog_rewound: false,
        })
    }

    /// `open`, reading only the segments from the one holding the entry at `from` on, and handing `visit`
    /// each entry from `from` on: a start from a daily snapshot, whose log before it need not be here.
    /// Error cases:
    /// - as `open_listed` with `from`
    pub fn open_from(
        dir: &Path,
        from: Offset,
        visit: impl FnMut(LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        Self::open_listed(dir, list_segments(dir)?, false, Some(from), crate::log::by_entry(visit))
    }
}

/// The index in `files` of the first segment a read from `from` reads: the one holding it, or the first,
/// for a read from the log's start or from before a log that starts later; either way, no listed segment
/// before the one it reads may be absent.
/// Error cases:
/// - the read starts before the first segment here and the manifest lists one before it (`NotLocal`)
pub(super) fn first_read(manifest: &Manifest, files: &[LocalSegment], from: Option<u64>) -> anyhow::Result<usize> {
    let first_local = files.first().map(|(first_seq, _, _)| *first_seq);
    if let Some(at) = from.and_then(|seq| files.iter().rposition(|(first_seq, _, _)| *first_seq <= seq)) {
        return Ok(at);
    }
    let absent = manifest.segments.first().filter(|listed| first_local.is_none_or(|first| listed.first_seq < first));
    match absent {
        Some(listed) => Err(NotLocal { seq: from.unwrap_or(listed.first_seq), first_local }.into()),
        None => Ok(0),
    }
}
