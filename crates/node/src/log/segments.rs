//! The log on disk: a directory of segment files, each named for the seq of its first command
//! (`00000000000000000000.jsonl`), and `manifest.jsonl`. The writer appends to the last segment
//! and starts a new one at a command once the current one holds `segment_commands` commands, so a
//! seq's entries never straddle two segments. A completed segment is never written again; the
//! manifest gets one line for it with its seq range, size, and SHA-256. Completed segments and the
//! manifest are the public archive. Recovery reads the log a segment at a time, a line at a time,
//! and hands on nothing of a completed segment before it matches its listing.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::log::catalog::{CheckpointLocator, Completion, LazyCatalogView};
use crate::log::catalog_head::CatalogDomain;
use crate::log::catalog_owner::CatalogOwner;
use crate::log::checked::{self, CheckedChunks};
use crate::log::{ActivePrefix, LogEntry, Offset, ReadAuthority, offset_of, parse_line, read_lines};

pub const MANIFEST: &str = "manifest.jsonl";

/// The completed segments, oldest first, and the last entry the writer has flushed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    /// Populated only by strict full audit or an explicit listing. Empty on lazy recovery. Shared,
    /// so every copy of the published view, one per reader and segment, costs the same however
    /// many segments it lists.
    pub segments: Arc<Vec<SegmentInfo>>,
    pub catalog: Option<Arc<LazyCatalogView>>,
    pub active: Option<ActivePrefix>,
    pub authority: Option<ReadAuthority>,
    /// The offset of the last entry the writer has flushed, which readers yield nothing after;
    /// `None` before the first. Only the writer's copy carries it: the file lists segments alone.
    pub flushed: Option<Offset>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentInfo {
    pub file: String,
    pub first_seq: u64,
    pub last_seq: u64,
    pub bytes: u64,
    /// Hex SHA-256 of the file.
    pub sha256: String,
}

/// A log directory read to its end, ready to resume writing.
pub struct SegmentedLog {
    pub(super) dir: PathBuf,
    /// The offset of the last entry.
    pub(super) last: Option<Offset>,
    pub(super) manifest: Manifest,
    /// Bytes of the manifest file up to the end of its last whole line.
    pub(super) manifest_len: u64,
    /// The last segment, when the manifest does not list it yet.
    pub(super) open: Option<Segment>,
    /// Only uncovered, verified suffix completions; never historical catalog hydration.
    pub(super) pending_catalog: Vec<Completion>,
    pub(super) catalog_owner: Option<CatalogOwner>,
    pub(super) catalog_domain: Option<CatalogDomain>,
    /// Recovery resumed the catalog from a cache's boundary, past which the log lost a tail.
    pub(super) catalog_rewound: bool,
}

/// Appends entries to the segment directory, buffered until `flush`.
pub struct LogWriter {
    pub(super) dir: PathBuf,
    pub(super) segment_commands: u64,
    /// Published for the archive and for stream readers, which trust it over the host's copy.
    pub(super) manifest: watch::Sender<Manifest>,
    /// Held from startup, so listing a segment needs no new file descriptor.
    pub(super) manifest_file: File,
    /// Physical bytes; legacy whitespace can differ from the public canonical listing length.
    pub(super) manifest_len: u64,
    pub(super) dir_handle: File,
    pub(super) open: Option<(Segment, BufWriter<File>)>,
    /// The offset of the last entry appended.
    pub(super) last: Option<Offset>,
    /// A new segment could not be created; the current one keeps taking appends meanwhile.
    pub(super) rotation_failing: bool,
    /// The open segment may hold bytes this writer has not synced: entries appended since its last
    /// sync, or a segment reopened from disk.
    pub(super) unsynced: bool,
    /// The catalog could not take a completed segment; its warning has been raised once.
    pub(super) catalog_failing: bool,
    pub(super) catalog_owner: Option<CatalogOwner>,
    /// File syncs since the writer opened, for sync-budget assertions in tests.
    #[cfg(test)]
    pub(super) syncs: usize,
}

/// What the manifest needs to know about a segment, kept up to date as it is written.
pub(super) struct Segment {
    pub(super) first_seq: u64,
    pub(super) last_seq: u64,
    pub(super) commands: u64,
    pub(super) bytes: u64,
    pub(super) hasher: Sha256,
    pub(super) checkpoints: Vec<CheckpointLocator>,
}

impl Manifest {
    /// Reads the manifest in `dir`; a missing one lists nothing.
    /// Error cases:
    /// - the file exists but cannot be read, or a whole line is not a valid listing
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        Self::read(dir).map(|(manifest, _)| manifest)
    }

    /// Encodes the explicit public listing in canonical ordinal order.
    /// Error cases:
    /// - a required proof fails or a listing cannot be encoded
    pub fn to_jsonl(&self) -> anyhow::Result<Vec<u8>> {
        self.canonical_jsonl()
    }

    /// The manifest in `dir`, and the length of its whole lines. A last line with no line break,
    /// which a crash while listing leaves, is left out: the segment it lists is then still the last
    /// one, and is listed again when the next segment starts.
    /// Error cases:
    /// - the file exists but cannot be read, or a whole line is not a valid listing
    fn read(dir: &Path) -> anyhow::Result<(Self, u64)> {
        let path = dir.join(MANIFEST);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Self::default(), 0)),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let whole = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let segments = bytes[..whole]
            .split(|b| *b == b'\n')
            .enumerate()
            .filter(|(_, line)| !line.trim_ascii().is_empty())
            .map(|(n, line)| serde_json::from_slice(line).with_context(|| format!("{} line {}", path.display(), n + 1)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok((Self { segments: Arc::new(segments), ..Self::default() }, whole as u64))
    }
}

impl SegmentedLog {
    /// Reads every segment in `dir` in order, a line at a time, handing each entry to `visit`. A
    /// completed segment is checked against its manifest listing before any of it reaches `visit`;
    /// the segment still being written has no listing. A missing directory is an empty log.
    /// Error cases:
    /// - the directory, the manifest, or a segment cannot be read
    /// - a segment ends mid-line, holds an invalid entry, or does not start with the command for
    ///   the seq it is named for
    /// - the manifest lists a missing segment, or one whose seq range, size, or hash differs
    /// - a segment other than the last is missing from the manifest, or an empty last segment is
    ///   not named for the next seq
    /// - `visit` fails
    pub fn open(dir: &Path, visit: impl FnMut(LogEntry) -> anyhow::Result<()>) -> anyhow::Result<Self> {
        Self::open_listed(dir, list_segments(dir)?, false, visit)
    }

    /// As `open`, while a writer appends and rotates, for history repair. Rotation lists a segment
    /// in the manifest before it links the next one, so the manifest read after the listing covers
    /// every listed file; segments it lists beyond them completed later and are not read.
    /// Error cases:
    /// - as `open`
    pub fn open_while_writing(dir: &Path, visit: impl FnMut(LogEntry) -> anyhow::Result<()>) -> anyhow::Result<Self> {
        Self::open_listed(dir, list_segments(dir)?, true, visit)
    }

    /// As `open`, over segment files listed before the manifest is read; `writing` drops manifest
    /// entries beyond those files.
    /// Error cases:
    /// - as `open`
    pub(super) fn open_listed(
        dir: &Path,
        files: Vec<(u64, PathBuf)>,
        writing: bool,
        mut visit: impl FnMut(LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let (mut manifest, manifest_len) = Manifest::read(dir)?;
        if writing && manifest.segments.len() > files.len() {
            Arc::make_mut(&mut manifest.segments).truncate(files.len());
        }
        ensure!(
            manifest.segments.len() <= files.len(),
            "{MANIFEST} lists {} segments but {} are in {}",
            manifest.segments.len(),
            files.len(),
            dir.display()
        );
        let (mut last, mut open) = (None, None);
        for (i, (first_seq, path)) in files.iter().enumerate() {
            let listed = manifest.segments.get(i);
            if listed.is_none() && i + 1 < files.len() {
                bail!("{} is followed by another segment but is not in {MANIFEST}", path.display());
            }
            let segment = Segment::read(*first_seq, path, listed, &mut last, &mut visit)
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
        if let Some(segment) = open.as_ref().filter(|s| s.commands == 0) {
            let next_seq = last.map_or(0, |last: Offset| last.seq + 1);
            ensure!(segment.first_seq == next_seq, "empty last segment is not named for the next seq {next_seq}");
        }
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

    /// Whether the log's last segment is unlisted and holds entries: a follower's own tail,
    /// which no archive listing may extend across.
    pub fn has_open_tail(&self) -> bool {
        self.manifest.active.is_some()
    }

    /// The last completed segment the manifest lists, if any: a handover reads it for the final
    /// segment the successor lacks.
    pub fn last_listed_segment(&self) -> Option<SegmentInfo> {
        self.manifest.segments.last().cloned()
    }

    /// The seq of the last entry the log holds, if any.
    pub fn last_seq(&self) -> Option<u64> {
        self.last.map(|last| last.seq)
    }

    /// A writer that appends to the last segment, or starts a new one at the next command if the
    /// manifest lists the last one. It publishes the manifest on `manifest`, starting now, with
    /// everything read counted as flushed. An earlier process may have died before syncing what it
    /// wrote, so the last segment and the directory are synced first, and the open segment starts
    /// marked unsynced, so a flush with no new append still syncs it before publishing a cursor.
    /// Error cases:
    /// - the directory cannot be created, opened, or synced
    /// - the manifest cannot be trimmed to its whole lines, or opened for appending
    /// - the last segment cannot be opened for appending or synced, or a pending suffix catalog
    ///   cannot be prepared
    pub fn into_writer(
        mut self,
        segment_commands: u64,
        manifest: watch::Sender<Manifest>,
    ) -> anyhow::Result<LogWriter> {
        ensure!(
            self.manifest.catalog.is_none() || self.catalog_owner.is_some(),
            "post-preflight catalog owner required"
        );
        let open = match self.open.take() {
            Some(segment) => {
                // A crash between linking this segment and removing its staging name leaves that
                // name; any other staging file is the next rotation's, which replaces it.
                crate::log::writer::remove_if_present(&crate::log::writer::staging_path(&self.dir, segment.first_seq))
                    .context("removing a staged segment name")?;
                let output = crate::log::append_integrity::open_active(&self.dir, &segment)?;
                let path = self.dir.join(segment_name(segment.first_seq));
                output.get_ref().sync_data().with_context(|| format!("syncing {}", path.display()))?;
                Some((segment, output))
            }
            None => None,
        };
        self.prepare_catalog()?;
        fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        // Drop a listing a crash cut short, so the next one starts on its own line.
        let manifest_path = self.dir.join(MANIFEST);
        if fs::metadata(&manifest_path).is_ok_and(|m| m.len() > self.manifest_len) {
            OpenOptions::new()
                .write(true)
                .open(&manifest_path)
                .and_then(|f| f.set_len(self.manifest_len))
                .with_context(|| format!("trimming {}", manifest_path.display()))?;
        }
        let manifest_file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&manifest_path)
            .with_context(|| format!("opening {}", manifest_path.display()))?;
        ensure!(manifest_file.metadata()?.len() == self.manifest_len, "manifest changed before recovered append");
        let dir_handle = File::open(&self.dir).with_context(|| format!("opening {}", self.dir.display()))?;
        dir_handle.sync_all().with_context(|| format!("syncing {}", self.dir.display()))?;
        manifest.send_replace(Manifest { flushed: self.last, ..self.manifest });
        let (last, rotation_failing, unsynced) = (self.last, false, open.is_some());
        Ok(LogWriter {
            dir: self.dir,
            segment_commands,
            manifest,
            manifest_file,
            manifest_len: self.manifest_len,
            dir_handle,
            open,
            last,
            rotation_failing,
            unsynced,
            catalog_failing: false,
            catalog_owner: self.catalog_owner,
            #[cfg(test)]
            syncs: 0,
        })
    }
}

impl Segment {
    /// A new, empty segment for `first_seq`.
    pub(super) fn empty(first_seq: u64) -> Self {
        Self { first_seq, last_seq: first_seq, commands: 0, bytes: 0, hasher: Sha256::new(), checkpoints: Vec::new() }
    }

    /// Reads the segment named for `first_seq` at `path` a line at a time, handing each entry to
    /// `visit`. A completed segment, `listed` in the manifest, is checked against its listing in one
    /// pass first, then read in chunks checked against that pass, since the host can rewrite it in
    /// between. `last` is the offset of the entry before the segment, and ends at its last entry.
    /// Error cases:
    /// - the file cannot be read, or ends mid-line, which a torn write leaves
    /// - a listed segment's size or hash differs from its listing
    /// - a non-blank line is not a valid entry, or the first entry is not the command for
    ///   `first_seq`
    /// - `visit` fails
    fn read(
        first_seq: u64,
        path: &Path,
        listed: Option<&SegmentInfo>,
        last: &mut Option<Offset>,
        visit: &mut impl FnMut(LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let input: Box<dyn BufRead> = match listed {
            Some(listed) => Box::new(BufReader::new(CheckedChunks::open(path, checked::check(path, listed)?, 0)?)),
            None => Box::new(BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?)),
        };
        let mut segment = Self::empty(first_seq);
        read_lines(input, |n, line| {
            ensure!(line.ends_with(b"\n"), "ends mid-line");
            let entry = parse_line(n, line)?;
            if let Some(entry) = &entry
                && segment.commands == 0
            {
                ensure!(command_seq(entry) == Some(first_seq), "does not start with the command for seq {first_seq}");
            }
            if let Some(LogEntry::Checkpoint(checkpoint)) = &entry {
                let offset = offset_of(*last, entry.as_ref().expect("checkpoint matched above"))?;
                segment.checkpoints.push(CheckpointLocator {
                    segment_first_seq: first_seq,
                    offset,
                    start: segment.bytes,
                    end: segment.bytes + line.len() as u64,
                    checkpoint: checkpoint.clone(),
                });
            }
            segment.record(line, entry.as_ref());
            let Some(entry) = entry else {
                return Ok(());
            };
            *last = Some(offset_of(*last, &entry)?);
            visit(entry)
        })?;
        Ok(segment)
    }

    /// Accounts for one line, and the entry on it unless it is blank.
    pub(super) fn record(&mut self, line: &[u8], entry: Option<&LogEntry>) {
        self.hasher.update(line);
        self.bytes += line.len() as u64;
        if let Some(seq) = entry.and_then(command_seq) {
            self.commands += 1;
            self.last_seq = seq;
        }
    }

    /// The manifest entry for this segment as written so far.
    pub(super) fn info(&self) -> SegmentInfo {
        SegmentInfo {
            file: segment_name(self.first_seq),
            first_seq: self.first_seq,
            last_seq: self.last_seq,
            bytes: self.bytes,
            sha256: hex::encode(self.hasher.clone().finalize()),
        }
    }
}

/// The segment files in `dir`, by first seq; a missing directory has none.
/// Error cases:
/// - the directory exists but cannot be listed
pub(super) fn list_segments(dir: &Path) -> anyhow::Result<Vec<(u64, PathBuf)>> {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    let mut segments = Vec::new();
    for item in listing {
        let name = item.with_context(|| format!("listing {}", dir.display()))?.file_name();
        if let Some(seq) = name.to_str().and_then(segment_seq) {
            segments.push((seq, dir.join(name)));
        }
    }
    segments.sort_unstable_by_key(|(seq, _)| *seq);
    Ok(segments)
}

/// One manifest line.
/// Error cases:
/// - the listing cannot be encoded
pub(super) fn listing_line(listed: &SegmentInfo) -> anyhow::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(listed).context("encoding segment listing")?;
    line.push(b'\n');
    Ok(line)
}

/// The file name of the segment whose first command is `first_seq`.
pub(super) fn segment_name(first_seq: u64) -> String {
    format!("{first_seq:020}.jsonl")
}

/// The first seq a segment file name stands for, if it is one.
pub(crate) fn segment_seq(name: &str) -> Option<u64> {
    let seq = name.strip_suffix(".jsonl")?.parse().ok()?;
    (segment_name(seq) == name).then_some(seq)
}

/// The seq of a command entry.
pub(super) fn command_seq(entry: &LogEntry) -> Option<u64> {
    match entry {
        LogEntry::Command(c) => Some(c.sequenced.seq),
        _ => None,
    }
}

#[cfg(test)]
mod durability_tests;
