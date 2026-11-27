//! What holds a segment on the host once its every command is older than the window: the seqs, read from the log's
//! own commands and the node's own files and never from the host's clock, before which a segment may leave.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use engine_core::types::Millis;
use sha2::{Digest, Sha256};

use crate::config::HistorySection;
use crate::daily_snapshots::{self, Listed};
use crate::history::HistoryPlan;
use crate::log::{LogEntry, Manifest, SegmentInfo, list_segments, open_segment_archive};

/// Why a segment past the window stays on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Held {
    /// A command of it is not older than the window.
    Window,
    /// The bucket does not hold every range it holds seqs of yet, which `archive_upload_lagging` reports.
    Archive,
    /// It holds a seq of a daily snapshot a start may go on from, or of one after it, or the node holds none.
    Snapshot,
    /// The history index re-derives a retained command's rows from a daily snapshot at or before it, or no snapshot
    /// is at or before the index's retention.
    History,
    /// A reader of the log holds it.
    Reader,
    /// The bucket did not give back the bytes its listing names.
    ReadBack,
}

/// The stamp of each segment's first command read so far, by its first seq and hash.
#[derive(Default)]
pub(crate) struct Stamps(BTreeMap<(u64, String), Millis>);

/// The seqs before which a segment may leave the host, as the node's daily snapshots and history index set them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Floors {
    /// The checkpoint of the newest daily snapshot a start may go on from (`daily_snapshots::newest_candidate`).
    pub snapshot: Option<u64>,
    /// The checkpoint of the newest daily snapshot stamped before the history index's retention.
    pub history: Option<u64>,
}

/// The daily snapshots whose file was read, by file, with the length and modification time it had then and whether it
/// hashed as its listing names it: one that changed since is read again, and one that did not is not.
#[derive(Default)]
pub(crate) struct Checked(BTreeMap<String, ((u64, Option<SystemTime>), bool)>);

/// Where a new or discarded history index takes a log whose first segment left the host from (`fresh_start`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FreshStart {
    /// The command after the checkpoint of the newest daily snapshot stamped before the retention, or the log's
    /// first when the host holds no such snapshot, as once its file left the host after the retention grew.
    pub from: u64,
    /// The first seq of the oldest segment on the host.
    pub first_local: u64,
}

impl Held {
    /// What holds the segment, as an alert's message names it.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Window => "its commands are within the window",
            Self::Archive => "the bucket does not hold every range of it yet",
            Self::Snapshot => "a daily snapshot a start may go on from is at or before it, or the node holds none",
            Self::History => "the history index re-derives rows from a daily snapshot at or before it",
            Self::Reader => "a reader of the log holds it",
            Self::ReadBack => "the bucket does not give back the bytes its listing names",
        }
    }
}

impl Stamps {
    /// The stamp of `listed`'s first command, read once from its file in `dir`, checked against its listing.
    /// Error cases:
    /// - see `first_stamp`
    pub(crate) fn of(&mut self, dir: &Path, listed: &SegmentInfo) -> anyhow::Result<Millis> {
        let key = (listed.first_seq, listed.sha256.clone());
        if let Some(ts) = self.0.get(&key) {
            return Ok(*ts);
        }
        let ts = first_stamp(dir, listed)?;
        self.0.insert(key, ts);
        Ok(ts)
    }

    /// Forgets the stamps of segments before `seq`, which have left the host.
    pub(crate) fn forget_before(&mut self, seq: u64) {
        self.0 = self.0.split_off(&(seq, String::new()));
    }
}

impl Floors {
    /// The floors the daily snapshots in `snapshots` and the history index in `history` set, the newest command
    /// stamped `now` or after: a start goes on from a daily snapshot at or before `bound`, a seq at or before the
    /// vault's finalized checkpoint, at or before the newest whose frozen marks copy the node holds, and at or before
    /// the index's last command, as a member's start takes it; the index re-derives a command's rows from the newest
    /// snapshot before it, so the one before its retention's start holds every retained command's, as does a
    /// member's derivation of the rows before its replica's snapshot (`RowsStore::derive_retained`). A snapshot
    /// counts for the history only once its file hashes as its listing names it, read once (`checked`). The index
    /// is read as a start plans it, beside no writer: on a member between its turns, whose start that bound is for,
    /// none runs, and a live index's last command is past every candidate.
    pub(crate) fn read(
        snapshots: &Path,
        history: &HistorySection,
        bound: u64,
        now: Millis,
        checked: &mut Checked,
    ) -> Self {
        let index = HistoryPlan::open(history).ok().and_then(|plan| plan.last_seq());
        let framed = daily_snapshots::framed(snapshots, &history.dir);
        let bound = [Some(bound), framed, index].into_iter().flatten().min().unwrap_or(bound);
        let snapshot = daily_snapshots::newest_candidate(snapshots, bound);
        let retained_from = log_time(snapshots, now).saturating_sub(millis(history.retention));
        Self { snapshot, history: held_before(snapshots, retained_from, Some(checked)) }
    }

    /// What holds a segment ending at `last_seq` on the host, if either floor does: it may leave only before both.
    pub(crate) fn holding(&self, last_seq: u64) -> Option<Held> {
        if self.snapshot.is_none_or(|floor| last_seq >= floor) {
            return Some(Held::Snapshot);
        }
        if self.history.is_none_or(|floor| last_seq >= floor) {
            return Some(Held::History);
        }
        None
    }
}

impl Checked {
    /// Whether the file of `listing` in `snapshots`, `file` its metadata, hashes as the listing names it, read only
    /// when it is new here or changed since.
    fn hashes(&mut self, snapshots: &Path, listing: &Listed, file: &std::fs::Metadata) -> bool {
        let seen = (file.len(), file.modified().ok());
        if let Some((_, whole)) = self.0.get(&listing.file).filter(|(read, _)| *read == seen) {
            return *whole;
        }
        let read = std::fs::File::open(snapshots.join(&listing.file)).and_then(|mut file| {
            let (mut hash, mut chunk) = (Sha256::new(), vec![0; 1 << 20]);
            loop {
                match file.read(&mut chunk)? {
                    0 => return Ok(hex::encode(hash.finalize())),
                    read => hash.update(&chunk[..read]),
                }
            }
        });
        let whole = read.is_ok_and(|sha256| sha256 == listing.sha256);
        if !whole {
            tracing::warn!(snapshot = %listing.file, "a daily snapshot's file does not hash as its listing names it");
        }
        self.0.insert(listing.file.clone(), (seen, whole));
        whole
    }
}

/// Where a new or discarded history index takes the log in `log` from, as its writer's `view` lists it, under the
/// daily snapshots in `snapshots` and the index's `retention`: none, the log's start, while the host holds the log's
/// first segment, which one look finds; otherwise the command after the checkpoint of the newest daily snapshot stamped
/// before the retention, counted back from the newest segment's first command, from which the window keeps every
/// segment (`Floors`), so the index re-derives every row it takes from the log on the host, or the log's first command
/// when the host holds no such snapshot, from which a re-derivation then replays. The host lacks the log from there
/// when the retention grew past what the window kept (`restore::history_start`). Every command before it is past the
/// retention.
/// Error cases:
/// - a listing, the log's directory, or its newest segment's first command cannot be read
pub(crate) fn fresh_start(
    view: &Manifest,
    log: &Path,
    snapshots: Option<&Path>,
    retention: Duration,
) -> anyhow::Result<Option<FreshStart>> {
    let Some(newest) = view.completed_len().checked_sub(1) else { return Ok(None) };
    let first = view.completed_at(0)?;
    if log.join(&first.file).try_exists().with_context(|| format!("probing {}", first.file))? {
        return Ok(None);
    }
    let Some(first_local) = list_segments(log)?.first().map(|(seq, _)| *seq) else { return Ok(None) };
    let newest = view.completed_at(newest)?;
    let floor = match snapshots.filter(|_| log.join(&newest.file).exists()) {
        Some(snapshots) => {
            let retained_from = log_time(snapshots, first_stamp(log, &newest)?).saturating_sub(millis(retention));
            held_before(snapshots, retained_from, None).map(|seq| seq.saturating_add(1))
        }
        None => None,
    };
    Ok(Some(FreshStart { from: floor.unwrap_or(0), first_local }))
}

/// The log's sequenced time as the history's floors count it: `now`, the newest completed segment's first command, or
/// the newest daily snapshot listed in `snapshots` when that is later, as in the minutes after one is taken, from
/// which the daily snapshots' own window counts the one it keeps before its retention.
fn log_time(snapshots: &Path, now: Millis) -> Millis {
    let listed = daily_snapshots::read_manifest(snapshots).unwrap_or_default();
    listed.iter().map(|listing| listing.ts).max().map_or(now, |newest| newest.max(now))
}

/// The checkpoint seq of the newest daily snapshot listed in `snapshots` under its own name, stamped before `ts`,
/// whose file is there, as long as its listing says, and with `checked` hashing as it says: a re-derivation that
/// cannot load the one it starts from falls back to an earlier one, whose log the window may no longer hold.
fn held_before(snapshots: &Path, ts: Millis, mut checked: Option<&mut Checked>) -> Option<u64> {
    let held = daily_snapshots::read_manifest(snapshots).unwrap_or_default();
    let before = held.iter().rev().filter(|listing| listing.ts < ts && daily_snapshots::canonical(listing));
    for listing in before {
        let Ok(file) = std::fs::metadata(snapshots.join(&listing.file)) else { continue };
        let hashes = |checked: &mut Checked| checked.hashes(snapshots, listing, &file);
        if file.len() == listing.bytes && checked.as_deref_mut().is_none_or(hashes) {
            return Some(listing.seq);
        }
    }
    None
}

/// `duration` in milliseconds, saturating.
pub(crate) fn millis(duration: Duration) -> Millis {
    Millis::try_from(duration.as_millis()).unwrap_or(Millis::MAX)
}

/// The stamp of the first command of `listed`, read from its file in `dir` checked against its listing.
/// Error cases:
/// - the file is not there or differs from its listing (`open_segment_archive`), or its first entry is not a command
fn first_stamp(dir: &Path, listed: &SegmentInfo) -> anyhow::Result<Millis> {
    let mut lines = BufReader::new(open_segment_archive(dir, listed)?);
    let mut line = Vec::new();
    loop {
        line.clear();
        if lines.read_until(b'\n', &mut line).with_context(|| format!("reading {}", listed.file))? == 0 {
            bail!("{} holds no command", listed.file);
        }
        if line.trim_ascii().is_empty() {
            continue;
        }
        return match serde_json::from_slice(&line).with_context(|| format!("the first line of {}", listed.file))? {
            LogEntry::Command(command) => Ok(command.sequenced.ts),
            _ => bail!("{} does not start with a command", listed.file),
        };
    }
}
