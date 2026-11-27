//! The log on disk: a directory of segment files, each named for the seq of its first command
//! (`00000000000000000000.jsonl`), and `manifest.jsonl`. The writer appends to the last segment
//! and starts a new one at a command once the current one holds `segment_commands` commands, or
//! would pass `MAX_SEGMENT_BYTES` with that command's line, so a seq's entries never straddle two
//! segments and a member fetches any segment whole. A completed segment is never written again; the
//! manifest gets one line for it with its seq range, size, and SHA-256. Completed segments and the
//! manifest are the public archive. Recovery reads the log a segment at a time, a line at a time,
//! and hands on nothing of a completed segment before it matches its listing.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::log::catalog::{CheckpointLocator, Completion, LazyCatalogView};
use crate::log::catalog_head::CatalogDomain;
use crate::log::catalog_owner::CatalogOwner;
use crate::log::checked::{self, CheckedChunks};
use crate::log::writer::seal::SealWork;
use crate::log::{ActivePrefix, Holds, LogEntry, Offset, ReadAuthority, by_entry, offset_of, parse_line, read_lines};

pub const MANIFEST: &str = "manifest.jsonl";

/// The most bytes a segment holds before a command whose line would take it past them starts the next: half of
/// what a member's fetch of one segment reads (`standby::MAX_ARCHIVE_BYTES`), so a segment's last command line and
/// the entries after it fit too. Only a carried genesis's parts, whose number its source sets, near it; a single
/// line larger than it still takes a segment of its own.
pub(crate) const MAX_SEGMENT_BYTES: u64 = crate::standby::MAX_ARCHIVE_BYTES / 2;

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
    /// The first offset this node's start verified, with no signed catalog: one that went on from a daily snapshot
    /// replayed nothing before its checkpoint, so it starts at the entry after it, and checked only the commands
    /// before it whose links the snapshot carries (`sequencer::replay::verified_back`), which lower it to the first
    /// of them, so the segments before those are the host's word, and neither a stream's backfill nor a financing
    /// read's rows file, which the enclave vouches for, reads them (`ensure_served`); the archive, whose answers are
    /// not signed, serves them as the host holds them, and the history index, which trusts the host's disk, reads
    /// them. The log's start for a start that read every segment, or one whose catalog vouches for them.
    pub verified_from: Offset,
    /// The segments readers of this log hold, which no segment they hold leaves the host while they do: one
    /// registry for each log opened, which its writer's every published view shares.
    pub holds: Holds,
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
    pub(super) open: Option<(Segment, crate::log::writer::output::Output)>,
    /// The offset of the last entry appended.
    pub(super) last: Option<Offset>,
    /// A new segment could not be created; the current one keeps taking appends meanwhile.
    pub(super) rotation_failing: bool,
    /// The open segment may hold bytes this writer has not synced: entries appended since its last
    /// sync, or a segment reopened from disk.
    pub(super) unsynced: bool,
    /// Entries appended since the writer opened: a `SyncJob` that covers the latest count leaves
    /// nothing unsynced.
    pub(super) appended: u64,
    /// `appended` when `begin_sync` last took the open segment's entries, if it has.
    pub(super) begun: Option<u64>,
    /// Bytes of the entries appended since the writer opened, and their count at the last
    /// `begin_sync`: what each sync job carries.
    pub(super) appended_bytes: u64,
    pub(super) begun_bytes: u64,
    /// The catalog could not take a completed segment; its warning has been raised once.
    pub(super) catalog_failing: bool,
    pub(super) catalog_owner: Option<CatalogOwner>,
    /// Whether rotation defers its seal to the log-sync thread (`pipeline_log_syncs`).
    pub(super) seals_deferred: bool,
    /// Segments rotated but not yet listed: each `SealWork` rides the next `SyncJob` as the job's
    /// `seals`, and `finish_sync` publishes it once the sync thread has run it. The manifest length
    /// counts each of them from defer time on.
    pub(super) pending_seals: Vec<SealWork>,
    /// Seals `begin_sync` has taken into sync jobs that have not published yet: with
    /// `pending_seals`, they keep a direct flush or a checkpoint capture from treating the
    /// manifest as settled before the old segment's completion is out.
    pub(super) taken_seals: u64,
    /// Sync jobs taken with entries or a seal and not finished: an inline write must wait for them.
    pub(super) handed: u64,
    /// A listing failed and the manifest could not be cut back to its durable length, so the
    /// listing may be on disk: nothing more is appended to the segment it lists.
    pub(super) listing_unsettled: bool,
    /// File syncs since the writer opened, for sync-budget assertions in tests.
    #[cfg(test)]
    pub(super) syncs: usize,
    /// Makes the next listing's manifest sync fail, after the line is written.
    #[cfg(test)]
    pub(super) fail_manifest_sync: bool,
}

/// What the manifest needs to know about a segment, kept up to date as it is written.
pub(super) struct Segment {
    pub(super) first_seq: u64,
    pub(super) last_seq: u64,
    pub(super) commands: u64,
    pub(super) bytes: u64,
    pub(super) hasher: Sha256,
    pub(super) checkpoints: Vec<CheckpointLocator>,
    /// The length of an unterminated last line read and left out: a write under way, or cut short.
    pub(super) torn: u64,
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
    pub(super) fn read(dir: &Path) -> anyhow::Result<(Self, u64)> {
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
    /// the segment still being written has no listing, and a last line of it with no line break,
    /// which a crash while appending leaves, is left out. A missing directory is an empty log.
    /// Error cases:
    /// - the directory, the manifest, or a segment cannot be read
    /// - a completed segment ends mid-line, or a segment holds an invalid entry, or does not start
    ///   with the command for the seq it is named for
    /// - the manifest lists a missing segment, or one whose seq range, size, or hash differs
    /// - a segment other than the last is missing from the manifest, or an empty last segment is
    ///   not named for the next seq
    /// - `visit` fails
    pub fn open(dir: &Path, visit: impl FnMut(LogEntry) -> anyhow::Result<()>) -> anyhow::Result<Self> {
        Self::open_listed(dir, list_segments(dir)?, false, None, by_entry(visit))
    }

    /// As `open`, while a writer appends and rotates, for history repair. Rotation lists a segment
    /// in the manifest before it links the next one, so the manifest read after the listing covers
    /// every listed file; segments it lists beyond them completed later and are not read.
    /// Error cases:
    /// - as `open`
    pub fn open_while_writing(dir: &Path, visit: impl FnMut(LogEntry) -> anyhow::Result<()>) -> anyhow::Result<Self> {
        Self::open_listed(dir, list_segments(dir)?, true, None, by_entry(visit))
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
    /// wrote, so a last line it left torn is dropped, the last segment and the directory are synced
    /// first, and the open segment starts marked unsynced, so a flush with no new append still syncs
    /// it before publishing a cursor.
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
                let file = crate::log::append_integrity::open_active(&self.dir, &segment)?;
                let path = self.dir.join(segment_name(segment.first_seq));
                file.sync_data().with_context(|| format!("syncing {}", path.display()))?;
                Some((segment, crate::log::writer::output::Output::new(file)))
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
            appended: 0,
            appended_bytes: 0,
            begun_bytes: 0,
            begun: None,
            catalog_failing: false,
            catalog_owner: self.catalog_owner,
            seals_deferred: false,
            pending_seals: Vec::new(),
            taken_seals: 0,
            handed: 0,
            listing_unsettled: false,
            #[cfg(test)]
            syncs: 0,
            #[cfg(test)]
            fail_manifest_sync: false,
        })
    }
}

impl Segment {
    /// A new, empty segment for `first_seq`.
    pub(super) fn empty(first_seq: u64) -> Self {
        let (hasher, checkpoints) = (Sha256::new(), Vec::new());
        Self { first_seq, last_seq: first_seq, commands: 0, bytes: 0, hasher, checkpoints, torn: 0 }
    }

    /// Reads the segment named for `first_seq` at `path` a line at a time, handing each entry to
    /// `visit`. A completed segment, `listed` in the manifest, is checked against its listing in one
    /// pass first, then read in chunks checked against that pass, since the host can rewrite it in
    /// between. `last` is the offset of the entry before the segment, and ends at its last entry.
    /// The writer appends whole lines and syncs a line only once its break is written, so a last line
    /// with no break, in a segment not yet listed, is a write still under way or cut short by a crash,
    /// never durable: it is left out, as the manifest's is, and `into_writer` drops it. A listed
    /// segment was synced whole.
    /// Error cases:
    /// - the file cannot be read, or is listed and ends mid-line
    /// - a listed segment's size or hash differs from its listing
    /// - a non-blank line is not a valid entry, or the first entry is not the command for
    ///   `first_seq`
    /// - `visit` fails
    pub(super) fn read(
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
            if !line.ends_with(b"\n") {
                ensure!(listed.is_none(), "ends mid-line");
                segment.torn = line.len() as u64;
                return Ok(());
            }
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
            segment.record(line, entry.as_ref().and_then(command_seq));
            let Some(entry) = entry else {
                return Ok(());
            };
            *last = Some(offset_of(*last, &entry)?);
            visit(entry)
        })?;
        Ok(segment)
    }

    /// Accounts for one line, and for the command on it, at `command`, if it holds one.
    pub(super) fn record(&mut self, line: &[u8], command: Option<u64>) {
        self.hasher.update(line);
        self.bytes += line.len() as u64;
        if let Some(seq) = command {
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
pub(crate) fn list_segments(dir: &Path) -> anyhow::Result<Vec<(u64, PathBuf)>> {
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
pub(crate) fn segment_name(first_seq: u64) -> String {
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
