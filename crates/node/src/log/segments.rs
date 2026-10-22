//! The log on disk: a directory of segment files, each named for the seq of its first command
//! (`00000000000000000000.jsonl`), and `manifest.jsonl`. The writer appends to the last segment
//! and starts a new one at a command once the current one holds `segment_commands` commands, so a
//! seq's entries never straddle two segments. A completed segment is never written again; the
//! manifest gets one line for it with its seq range, size, and SHA-256. Completed segments and the
//! manifest are the public archive. Recovery reads the log a segment at a time, a line at a time,
//! and hands on nothing of a completed segment before it matches its listing.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::log::checked::{self, CheckedChunks};
use crate::log::{LogEntry, Offset, offset_of, parse_line, read_lines};

pub const MANIFEST: &str = "manifest.jsonl";

/// The completed segments, oldest first, and the last entry the writer has flushed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    pub segments: Vec<SegmentInfo>,
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
    dir: PathBuf,
    /// The offset of the last entry.
    last: Option<Offset>,
    manifest: Manifest,
    /// Bytes of the manifest file up to the end of its last whole line.
    manifest_len: u64,
    /// The last segment, when the manifest does not list it yet.
    open: Option<Segment>,
}

/// Appends entries to the segment directory, buffered until `flush`.
pub struct LogWriter {
    dir: PathBuf,
    segment_commands: u64,
    /// Published for the archive and for stream readers, which trust it over the host's copy.
    manifest: watch::Sender<Manifest>,
    /// Held from startup, so listing a segment needs no new file descriptor.
    manifest_file: File,
    dir_handle: File,
    open: Option<(Segment, BufWriter<File>)>,
    /// The offset of the last entry appended.
    last: Option<Offset>,
    /// A new segment could not be created; the current one keeps taking appends meanwhile.
    rotation_failing: bool,
    /// Entries were appended since the open segment was last synced.
    pub(super) unsynced: bool,
}

/// What the manifest needs to know about a segment, kept up to date as it is written.
struct Segment {
    first_seq: u64,
    last_seq: u64,
    commands: u64,
    bytes: u64,
    hasher: Sha256,
}

impl Manifest {
    /// Reads the manifest in `dir`; a missing one lists nothing.
    /// Error cases:
    /// - the file exists but cannot be read, or a whole line is not a valid listing
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        Self::read(dir).map(|(manifest, _)| manifest)
    }

    /// The listing of the completed segment whose first command is `first_seq`.
    pub fn get(&self, first_seq: u64) -> Option<&SegmentInfo> {
        let i = self.segments.binary_search_by_key(&first_seq, |s| s.first_seq).ok()?;
        Some(&self.segments[i])
    }

    /// The manifest as it is stored: one JSON line per completed segment.
    /// Error cases:
    /// - a listing cannot be encoded
    pub fn to_jsonl(&self) -> anyhow::Result<Vec<u8>> {
        self.segments.iter().try_fold(Vec::new(), |mut out, listed| {
            out.extend(listing_line(listed)?);
            Ok(out)
        })
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
            .collect::<anyhow::Result<_>>()?;
        Ok((Self { segments, flushed: None }, whole as u64))
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
    pub fn open(dir: &Path, mut visit: impl FnMut(LogEntry) -> anyhow::Result<()>) -> anyhow::Result<Self> {
        let (manifest, manifest_len) = Manifest::read(dir)?;
        let files = list_segments(dir)?;
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
        Ok(Self { dir: dir.to_owned(), last, manifest, manifest_len, open })
    }

    /// A writer that appends to the last segment, or starts a new one at the next command if the
    /// manifest lists the last one. It publishes the manifest on `manifest`, starting now, with
    /// everything read counted as flushed. An earlier process may have died before syncing what it
    /// wrote, so the last segment and the directory are synced first.
    /// Error cases:
    /// - the directory cannot be created, opened, or synced
    /// - the manifest cannot be trimmed to its whole lines, or opened for appending
    /// - the last segment cannot be opened for appending, or synced
    pub fn into_writer(self, segment_commands: u64, manifest: watch::Sender<Manifest>) -> anyhow::Result<LogWriter> {
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
            .append(true)
            .open(&manifest_path)
            .with_context(|| format!("opening {}", manifest_path.display()))?;
        let dir_handle = File::open(&self.dir).with_context(|| format!("opening {}", self.dir.display()))?;
        dir_handle.sync_all().with_context(|| format!("syncing {}", self.dir.display()))?;
        let open = match self.open {
            Some(segment) => {
                let path = self.dir.join(segment_name(segment.first_seq));
                let file = OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("opening {}", path.display()))?;
                file.sync_data().with_context(|| format!("syncing {}", path.display()))?;
                Some((segment, BufWriter::new(file)))
            }
            None => None,
        };
        manifest.send_replace(Manifest { flushed: self.last, ..self.manifest });
        let (last, rotation_failing, unsynced) = (self.last, false, false);
        Ok(LogWriter {
            dir: self.dir,
            segment_commands,
            manifest,
            manifest_file,
            dir_handle,
            open,
            last,
            rotation_failing,
            unsynced,
        })
    }
}

impl LogWriter {
    /// Buffers one entry as a JSON line, and returns its offset. A command first completes the
    /// current segment if it is full.
    /// Error cases:
    /// - completing the current segment or starting a new one fails
    /// - a withdrawal or checkpoint comes before any command in this segment
    /// - the write to the buffer or file fails
    pub fn append(&mut self, entry: &LogEntry) -> anyhow::Result<Offset> {
        let offset = offset_of(self.last, entry)?;
        if let Some(seq) = command_seq(entry)
            && self.open.as_ref().is_none_or(|(segment, _)| segment.commands >= self.segment_commands)
        {
            self.start_segment(seq)?;
        }
        let (segment, out) = self.open.as_mut().context("log entry before the first command of its segment")?;
        let mut line = serde_json::to_vec(entry).context("encoding log entry")?;
        line.push(b'\n');
        // Set first: the buffer may reach the file before `flush`, and those bytes need syncing too.
        self.unsynced = true;
        out.write_all(&line).context("writing log entry")?;
        segment.record(&line, Some(entry));
        self.last = Some(offset);
        Ok(offset)
    }

    /// Writes buffered entries to the open segment and syncs it to the disk, unless nothing was
    /// appended since the last sync, then publishes the last of them as flushed. Called after each
    /// command batch: a command must be durable before its effects are acknowledged to clients or
    /// read from disk by the stream, and in a confidential VM the guest page cache is lost whenever
    /// the VM stops.
    /// Error cases:
    /// - the write to the file, or the sync, fails
    pub fn flush(&mut self) -> anyhow::Result<()> {
        if let Some((_, out)) = self.open.as_mut().filter(|_| self.unsynced) {
            out.flush().context("flushing log")?;
            out.get_ref().sync_data().context("syncing log")?;
            self.unsynced = false;
        }
        let last = self.last;
        self.manifest.send_modify(|published| published.flushed = last);
        Ok(())
    }

    /// Flushes, which syncs the open segment's data, then forces the directory entry naming it to
    /// storage, so its entries survive a host kill or a power loss as well as a process crash.
    /// Error cases:
    /// - the write or either sync fails
    pub fn sync(&mut self) -> anyhow::Result<()> {
        self.flush()?;
        self.dir_handle.sync_all().with_context(|| format!("syncing {}", self.dir.display()))
    }

    /// Starts the segment for `first_seq`, first syncing and listing the current one, if any. The
    /// new file takes its segment name only once the old one is listed, which `LogReader` relies on.
    /// Creating it is the only step that needs a new file descriptor, so it runs first: if it fails
    /// while a segment is open, that segment keeps taking appends and the next command retries.
    /// Error cases:
    /// - the new segment cannot be created and no segment is open
    /// - a segment by the new name already exists
    /// - the current segment cannot be flushed or synced, or its listing cannot be stored
    /// - the new segment cannot be renamed into place, or the directory naming it synced
    fn start_segment(&mut self, first_seq: u64) -> anyhow::Result<()> {
        let path = self.dir.join(segment_name(first_seq));
        let staged = self.dir.join(format!("{}.tmp", segment_name(first_seq)));
        let file = match OpenOptions::new().write(true).create(true).truncate(true).open(&staged) {
            Ok(file) => file,
            Err(e) if self.open.is_some() => {
                if !self.rotation_failing {
                    warn!(alert = "log_rotation_failed", "cannot start a log segment, so the current one grows: {e}");
                    self.rotation_failing = true;
                }
                return Ok(());
            }
            Err(e) => return Err(e).with_context(|| format!("creating {}", staged.display())),
        };
        ensure!(!path.exists(), "{} already exists", path.display());
        if let Some((segment, out)) = self.open.take() {
            let done = out.into_inner().map_err(|e| e.into_error()).context("flushing log segment")?;
            done.sync_all().context("syncing log segment")?;
            let listed = segment.info();
            self.list(&listed)?;
            info!(
                file = %listed.file,
                first_seq = listed.first_seq,
                last_seq = listed.last_seq,
                bytes = listed.bytes,
                sha256 = %listed.sha256,
                "log segment completed"
            );
            self.manifest.send_modify(|m| m.segments.push(listed));
        }
        fs::rename(&staged, &path).with_context(|| format!("renaming {} into place", staged.display()))?;
        self.open = Some((Segment::empty(first_seq), BufWriter::new(file)));
        // A flush syncs only the segment's data, so its name is made durable here.
        self.dir_handle.sync_all().with_context(|| format!("syncing {}", self.dir.display()))?;
        if std::mem::take(&mut self.rotation_failing) {
            info!(alert_resolved = "log_rotation_failed", "log segments rotate again");
        }
        Ok(())
    }

    /// Appends one listing to the manifest, durably, after the listed segment's directory entry.
    /// Error cases:
    /// - the directory or the manifest cannot be synced, or the manifest cannot be written
    fn list(&mut self, listed: &SegmentInfo) -> anyhow::Result<()> {
        self.dir_handle.sync_all().with_context(|| format!("syncing {}", self.dir.display()))?;
        self.manifest_file.write_all(&listing_line(listed)?).context("writing manifest")?;
        self.manifest_file.sync_all().context("syncing manifest")
    }
}

impl Segment {
    /// A new, empty segment for `first_seq`.
    fn empty(first_seq: u64) -> Self {
        Self { first_seq, last_seq: first_seq, commands: 0, bytes: 0, hasher: Sha256::new() }
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
    fn record(&mut self, line: &[u8], entry: Option<&LogEntry>) {
        self.hasher.update(line);
        self.bytes += line.len() as u64;
        if let Some(seq) = entry.and_then(command_seq) {
            self.commands += 1;
            self.last_seq = seq;
        }
    }

    /// The manifest entry for this segment as written so far.
    fn info(&self) -> SegmentInfo {
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
fn listing_line(listed: &SegmentInfo) -> anyhow::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(listed).context("encoding segment listing")?;
    line.push(b'\n');
    Ok(line)
}

/// The file name of the segment whose first command is `first_seq`.
fn segment_name(first_seq: u64) -> String {
    format!("{first_seq:020}.jsonl")
}

/// The first seq a segment file name stands for, if it is one.
pub(crate) fn segment_seq(name: &str) -> Option<u64> {
    let seq = name.strip_suffix(".jsonl")?.parse().ok()?;
    (segment_name(seq) == name).then_some(seq)
}

/// The seq of a command entry.
fn command_seq(entry: &LogEntry) -> Option<u64> {
    match entry {
        LogEntry::Command(c) => Some(c.sequenced.seq),
        _ => None,
    }
}
