//! The standby's cold start. The completed segments it backfills from the operator's archive, its
//! mirrors first, are only hash-checked against the manifest the answering origin served, so before
//! the standby follows onto them they must replay as its takeover will replay them (`replay_as_takeover`):
//! a takeover refuses a copy that does not, once the operator is gone. A copy that does not is set aside
//! and backfilled again from the operator in effect alone, paging Critical `standby_backfill_refused`, and
//! so is one a mirror cut short of its first checkpoint, which no replay can check. A copy is marked
//! checked only while every byte it holds after the replay's start has replayed or come from the operator's
//! signed stream. Each replay goes on from the newest daily snapshot this node holds whose checkpoint the copy
//! holds, at or before the vault's finalized checkpoint (`handover::copy_start`), and reads nothing of the copy
//! before it: those segments stay the mirror's word, which the node never serves once it sequences
//! (`log::Manifest::verified_from`), and which a whole replay of the copy, when no snapshot passes, checks.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Context;
use engine_core::types::AccountId;
use tracing::{error, info, warn};

use crate::daily_snapshots::Signed;
use crate::log::{CheckpointDomain, LogEntry, MANIFEST, Manifest, ReplayWorkers, SegmentedLog};
use crate::standby::archive::bootstrap_archive;
use crate::standby::operator::Followed;

mod trust;
pub(super) use trust::{Trust, read_trust};

/// Marks a copy that has replayed as a takeover would: a start that does not find it, after a crash
/// between a backfill and its check or on a copy an earlier release backfilled, checks the copy before
/// following onto it. A takeover's archive refresh removes it until its replay commits the refresh, and
/// a refresh it never commits is undone with the mark as it was (`begin_refresh`, `undo_refresh`).
const CHECKED: &str = "backfill.checked";

/// What a check found the copy to be.
#[derive(Debug, PartialEq)]
enum Found {
    /// It holds no entry: nothing to replay.
    Empty,
    /// It replays as a takeover would.
    Replays,
    /// It holds commands but no checkpoint, so no replay can check it: it is backfilled again from the
    /// operator, or refilled by the stream when the operator's copy holds no checkpoint either.
    Uncheckable,
}

/// The standby's cold start, before it follows onto the copy in `dir`: a takeover's cut or archive refresh
/// a crash interrupted is finished or dropped first; then a copy not marked checked (`needed`) is fetched
/// from `operator`'s archive when it lists nothing (`fetch`) and must replay as a takeover would, under what
/// `trust` reads once it is fetched (`cold_start`); a copy marked checked ends any `standby_backfill_refused`
/// episode (`resolve`).
/// Error cases:
/// - as `finish_cut`, `undo_refresh`, `fetch` and `cold_start`
pub(super) async fn prepare(
    archive: &reqwest::Client,
    operator: &Followed,
    dir: &Path,
    (domain, workers): (CheckpointDomain, ReplayWorkers),
    trust: impl AsyncFn() -> anyhow::Result<Trust>,
) -> anyhow::Result<()> {
    // A takeover's cut a crash interrupted is finished first, so the copy checked is the one followed,
    // and a refresh its replay never committed is dropped, so the copy is again what the stream gave.
    super::finish_cut(dir)?;
    if let Some(dropped) = undo_refresh(dir)? {
        warn!(dropped, "a takeover's archive refresh a crash interrupted is dropped from the copy");
    }
    if needed(dir) {
        // The signers are read once the copy is fetched, so a signer change during the fetch counts.
        fetch(archive, operator, dir).await?;
        cold_start(archive, operator, dir, (domain, workers), trust).await?;
    }
    resolve(dir);
    Ok(())
}

/// Where a takeover's archive refresh records what the copy held before it, until its replay commits it.
const REFRESH: &str = "refresh.before";

/// What the copy held before a takeover's archive refresh.
#[derive(serde::Serialize, serde::Deserialize)]
struct Before {
    /// Whether it was marked checked.
    checked: bool,
    /// Its segment files, listed or not.
    files: BTreeSet<String>,
    /// Its manifest.
    manifest: String,
}

/// Records what the copy in `dir` holds before a takeover's archive refresh adds to it, published whole,
/// and unmarks it: until the takeover's replay commits the refresh (`replay_refreshed`), every start
/// drops what it added (`undo_refresh`), after a crash included.
/// Error cases:
/// - the copy cannot be read, or the record written, or the mark removed
pub(super) fn begin_refresh(dir: &Path) -> anyhow::Result<()> {
    let manifest = match std::fs::read_to_string(dir.join(MANIFEST)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        read => read.context("reading the manifest")?,
    };
    let before = Before { checked: dir.join(CHECKED).exists(), files: segment_files(dir)?, manifest };
    publish(dir, REFRESH, &serde_json::to_vec(&before).context("encoding the refresh record")?)?;
    match std::fs::remove_file(dir.join(CHECKED)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        removed => removed.context("unmarking the copy")?,
    }
    sync(dir)
}

/// `replay_as_takeover` of the copy in `dir` after a takeover's archive refresh, from `start`, a daily snapshot
/// of the copy the stream gave, which the refresh only extends: a copy that replays is marked checked and the
/// refresh committed; one that does not has what the refresh added dropped (`undo_refresh`), and is marked
/// checked again if it was before, as what the operator's stream gave. When the refresh added segments, the copy
/// without them is then replayed in turn, from the same snapshot, opened again: an origin that served segments
/// no replay admits, as a mirror listed first can, leaves the takeover to what the stream gave.
/// Error cases:
/// - the copy does not replay, without the refresh's segments either, or cannot be marked or cut back
pub(super) fn replay_refreshed(
    dir: &Path,
    (domain, workers): (CheckpointDomain, ReplayWorkers),
    signers: BTreeSet<AccountId>,
    start: Option<Signed>,
) -> anyhow::Result<()> {
    let again = start.as_ref().map(|start| start.path.clone());
    match replay_as_takeover(dir, (domain, workers), signers.clone(), start) {
        Ok(()) => {
            forget(dir, REFRESH)?;
            mark(dir)
        }
        Err(refused) => match undo_refresh(dir) {
            Ok(Some(dropped)) if dropped > 0 => {
                warn!(
                    dropped,
                    "the takeover's replay refused the copy with the {dropped} segments the archive refresh \
                     added, which are dropped; replaying the copy as the operator's stream gave it: {refused:#}"
                );
                let start = again.and_then(|path| trust::reopen(&path, domain, &signers));
                replay_as_takeover(dir, (domain, workers), signers, start).and_then(|()| mark(dir)).map_err(|stream| {
                    stream.context(format!(
                        "the copy the operator's stream gave does not replay either, and the takeover's replay \
                         refused the {dropped} segments the archive refresh added ({refused:#})"
                    ))
                })
            }
            Ok(_) => Err(refused),
            Err(undo) => Err(refused.context(format!(
                "the copy does not replay, and dropping what the archive refresh added failed ({undo:#}): the next \
                 start drops it"
            ))),
        },
    }
}

/// Drops what a takeover's archive refresh never committed added to the copy in `dir`, if one is recorded:
/// the copy lists what it listed before, every segment file it did not hold is removed, newest first, and
/// it is marked checked again if it was. How many files were dropped, or `None` with no refresh recorded.
/// Error cases:
/// - the record or the copy cannot be read, or the manifest written, a file removed, or the copy marked
fn undo_refresh(dir: &Path) -> anyhow::Result<Option<usize>> {
    let record = match std::fs::read(dir.join(REFRESH)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        read => read.context("reading the refresh record")?,
    };
    let before: Before = serde_json::from_slice(&record).context("parsing the refresh record")?;
    publish(dir, MANIFEST, before.manifest.as_bytes())?;
    let added: Vec<String> = segment_files(dir)?.into_iter().filter(|file| !before.files.contains(file)).collect();
    for file in added.iter().rev() {
        std::fs::remove_file(dir.join(file)).with_context(|| format!("removing {file}"))?;
    }
    sync(dir)?;
    if before.checked {
        mark(dir)?;
    }
    forget(dir, REFRESH)?;
    Ok(Some(added.len()))
}

/// The segment files in `dir`, listed or not.
/// Error cases:
/// - the directory cannot be read
fn segment_files(dir: &Path) -> anyhow::Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();
    for file in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let name = file?.file_name().to_string_lossy().into_owned();
        if crate::log::segment_seq(&name).is_some() {
            files.insert(name);
        }
    }
    Ok(files)
}

/// Writes `bytes` to `name` in `dir` whole or not at all: staged, synced, renamed, the directory synced.
/// Error cases:
/// - a write, rename or sync fails
fn publish(dir: &Path, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let staged = dir.join(format!("{name}.staging"));
    std::fs::write(&staged, bytes).with_context(|| format!("writing {}", staged.display()))?;
    std::fs::File::open(&staged)?.sync_all().with_context(|| format!("syncing {}", staged.display()))?;
    std::fs::rename(&staged, dir.join(name)).with_context(|| format!("linking {name}"))?;
    sync(dir)
}

/// Removes `name` from `dir` for good, if it is there.
/// Error cases:
/// - the file cannot be removed, or the directory synced
fn forget(dir: &Path, name: &str) -> anyhow::Result<()> {
    match std::fs::remove_file(dir.join(name)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        removed => removed.with_context(|| format!("removing {name}")).and_then(|()| sync(dir)),
    }
}

/// Whether the copy in `dir` needs `fetch` and `cold_start` before the standby follows onto it: it is
/// not marked checked.
fn needed(dir: &Path) -> bool {
    !dir.join(CHECKED).exists()
}

/// Backfills the copy in `dir` from `operator`'s archive when it lists nothing yet: its mirrors first,
/// or the operator's own origin alone once a copy is set aside as refused, as a start after a rebuild
/// from the operator that did not finish must not take a refused mirror's copy again.
/// Error cases:
/// - the directory cannot be created, or the backfill fails
async fn fetch(archive: &reqwest::Client, operator: &Followed, dir: &Path) -> anyhow::Result<()> {
    if dir.join(MANIFEST).exists() {
        return Ok(());
    }
    match aside_of(dir, "refused").is_dir() {
        true => backfill(archive, std::slice::from_ref(&operator.origin), dir).await,
        false => backfill(archive, &operator.origins(), dir).await,
    }
}

/// Requires the copy in `dir` to replay in `domain` under what `trust` reads, as a takeover replays it, and
/// marks it checked. A copy that does not is set aside to `<dir>.refused` and backfilled
/// again from `operator` alone, paging Critical `standby_backfill_refused`; one that holds commands but no
/// checkpoint, as a mirror that cut it short of one serves, is set aside to `<dir>.unchecked` and
/// backfilled again from `operator` alone the same way, with no page. A copy that holds no entry, as a
/// fresh deployment's, has nothing to replay; one the operator serves that holds no checkpoint yet is set
/// aside to `<dir>.unchecked`, unmarked, for the operator's stream to refill from seq 0.
/// Error cases:
/// - `trust` cannot read the signers, the copy cannot be read, set aside or marked, or the backfill fails
/// - the copy the operator itself serves does not replay: it is set aside to `<dir>.refused-operator`
async fn cold_start(
    archive: &reqwest::Client,
    operator: &Followed,
    dir: &Path,
    (domain, workers): (CheckpointDomain, ReplayWorkers),
    trust: impl AsyncFn() -> anyhow::Result<Trust>,
) -> anyhow::Result<()> {
    let found = match check(dir, (domain, workers), trust().await?).await {
        Ok(Found::Uncheckable) => {
            set_aside(dir, "unchecked")?;
            from_operator(archive, operator, dir, (domain, workers), &trust).await?
        }
        Ok(found) => found,
        // With a copy set aside as refused, this one came from the operator's own origin (`fetch`).
        Err(refused) if aside_of(dir, "refused").is_dir() => return Err(refuse_operator_copy(operator, dir, refused)),
        Err(refused) => {
            error!(
                alert = "standby_backfill_refused",
                served_by = "archive",
                "the copy on disk, backfilled from the archive ({}) or left by an earlier start, does not \
                 replay as a takeover would replay it: {refused:#}; it is set aside to {} and backfilled again \
                 from the operator alone",
                operator.origins().join(", "),
                aside_of(dir, "refused").display()
            );
            set_aside(dir, "refused").with_context(|| format!("after refusing the copy: {refused:#}"))?;
            from_operator(archive, operator, dir, (domain, workers), &trust).await?
        }
    };
    if found == Found::Uncheckable {
        let aside = set_aside(dir, "unchecked")?;
        // The copy the standby follows onto is the operator's signed stream from seq 0 now.
        let aside = aside.display();
        end_refusals(&format!(
            "the copy holds no checkpoint to check yet; it is set aside to {aside} and the operator's stream \
             refills it from seq 0"
        ));
        return Ok(());
    }
    mark(dir)
}

/// Pages that the copy in `dir`, which `operator`'s own origin served, does not replay (`refused`), and
/// sets it aside to `<dir>.refused-operator`: the error that stops the start.
fn refuse_operator_copy(operator: &Followed, dir: &Path, refused: anyhow::Error) -> anyhow::Error {
    error!(
        alert = "standby_backfill_refused",
        served_by = "operator",
        "the copy the operator itself serves ({}) does not replay as a takeover would replay it: {refused:#}",
        operator.origin
    );
    let aside = match set_aside(dir, "refused-operator") {
        Ok(aside) => format!("it is set aside to {}", aside.display()),
        Err(e) => format!("it could not be set aside: {e:#}"),
    };
    refused.context(format!("the copy the operator itself serves does not replay; {aside}"))
}

/// Marks the copy in `dir` checked.
/// Error cases:
/// - the mark cannot be written, or the directory synced
fn mark(dir: &Path) -> anyhow::Result<()> {
    std::fs::File::create(dir.join(CHECKED)).and_then(|mark| mark.sync_all()).context("marking the copy checked")?;
    sync(dir)
}

/// Forces the directory's entries to storage.
/// Error cases:
/// - the directory cannot be opened or synced
fn sync(dir: &Path) -> anyhow::Result<()> {
    std::fs::File::open(dir)?.sync_all().with_context(|| format!("syncing {}", dir.display()))
}

/// Backfills the copy in `dir`, set aside, again from `operator`'s own origin alone and checks it under what
/// `trust` reads again: what it found.
/// Error cases:
/// - the backfill fails, the signers or the copy cannot be read, or the copy does not replay: it is set
///   aside to `<dir>.refused-operator`
async fn from_operator(
    archive: &reqwest::Client,
    operator: &Followed,
    dir: &Path,
    (domain, workers): (CheckpointDomain, ReplayWorkers),
    trust: &impl AsyncFn() -> anyhow::Result<Trust>,
) -> anyhow::Result<Found> {
    backfill(archive, std::slice::from_ref(&operator.origin), dir).await?;
    check(dir, (domain, workers), trust().await?).await.map_err(|refused| refuse_operator_copy(operator, dir, refused))
}

/// Ends the `standby_backfill_refused` episodes once the copy in `dir` is marked checked; with none open,
/// the router drops them. Every start reasserts it, so a resolution a crash lost is not lost for good.
pub(super) fn resolve(dir: &Path) {
    if dir.join(CHECKED).exists() {
        end_refusals("the standby's copy replays as a takeover would");
    }
}

/// Ends both `standby_backfill_refused` episodes, a copy the archive served and one the operator itself
/// served (`served_by`), which page apart so one never hides the other, saying `why`.
fn end_refusals(why: &str) {
    for served_by in ["archive", "operator"] {
        info!(alert_resolved = "standby_backfill_refused", served_by, "{why}");
    }
}

/// Replays the copy in `dir` as a takeover must before it truncates it: every checkpoint, typed
/// withdrawal and command seal from one of `signers`, in `domain`, the seals checked ahead by `workers`,
/// from `start`, a daily snapshot whose checkpoint the copy holds, or from the copy's first entry.
/// Error cases:
/// - the copy does not replay (see `replay_copy_from`)
pub(super) fn replay_as_takeover(
    dir: &Path,
    (domain, workers): (CheckpointDomain, ReplayWorkers),
    signers: BTreeSet<AccountId>,
    start: Option<Signed>,
) -> anyhow::Result<()> {
    crate::sequencer::replay_copy_from(start, dir, domain, signers, workers).map(drop)
}

/// `check_now` on a blocking thread, so a long replay holds up nothing the standby runs beside it, its
/// secrets listener or its seed relay.
/// Error cases:
/// - as `check_now`, or the thread stops
async fn check(
    dir: &Path,
    (domain, workers): (CheckpointDomain, ReplayWorkers),
    trust: Trust,
) -> anyhow::Result<Found> {
    info!(dir = %dir.display(), "replaying the standby's copy as a takeover would before following onto it");
    let dir = dir.to_owned();
    tokio::task::spawn_blocking(move || check_now(&dir, (domain, workers), trust))
        .await
        .context("checking the copy stopped")?
}

/// `replay_as_takeover` under `trust`, for a cold start: what the copy is, or why it does not replay.
/// Error cases:
/// - the copy cannot be read, or does not replay
fn check_now(dir: &Path, (domain, workers): (CheckpointDomain, ReplayWorkers), trust: Trust) -> anyhow::Result<Found> {
    if !holds_entries(dir)? {
        return Ok(Found::Empty);
    }
    let Trust { signers, start } = trust;
    // A copy that holds a snapshot's checkpoint holds a checkpoint: no read of the copy before it says so.
    let held = start.is_some();
    match replay_as_takeover(dir, (domain, workers), signers, start) {
        Ok(()) => Ok(Found::Replays),
        // A copy that cannot be read through is refused as one that does not replay.
        Err(refused) if held || holds_checkpoint(dir).unwrap_or(true) => Err(refused),
        Err(_) => Ok(Found::Uncheckable),
    }
}

/// Whether the copy in `dir` holds an entry, listed or in a segment file no listing names yet, from its
/// files alone.
/// Error cases:
/// - the manifest or the directory cannot be read
fn holds_entries(dir: &Path) -> anyhow::Result<bool> {
    if !Manifest::load(dir)?.segments.is_empty() {
        return Ok(true);
    }
    for file in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let file = file?;
        if crate::log::segment_seq(&file.file_name().to_string_lossy()).is_some() && file.metadata()?.len() > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the copy in `dir` holds a checkpoint.
/// Error cases:
/// - the copy cannot be read
fn holds_checkpoint(dir: &Path) -> anyhow::Result<bool> {
    let mut found = false;
    SegmentedLog::open(dir, |entry| {
        found |= matches!(entry, LogEntry::Checkpoint(_));
        Ok(())
    })?;
    Ok(found)
}

/// Backfills the copy in `dir` from `origins`.
/// Error cases:
/// - the directory cannot be created, or the backfill fails
async fn backfill(archive: &reqwest::Client, origins: &[String], dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    bootstrap_archive(archive, origins, dir).await
}

/// `<dir>.<suffix>`, beside `dir` however it is written.
fn aside_of(dir: &Path, suffix: &str) -> PathBuf {
    let name = dir.file_name().map_or_else(|| "log".into(), |name| name.to_string_lossy().into_owned());
    dir.parent().unwrap_or(Path::new("")).join(format!("{name}.{suffix}"))
}

/// Moves the copy in `dir` to `<dir>.<suffix>` and syncs the directory holding both: where it went. A copy
/// an earlier start set aside there is evidence, and may be the standby's own: the move fails, leaving
/// both, unless `suffix` is `unchecked`, a copy nothing could check, which this one replaces.
/// Error cases:
/// - an earlier copy is set aside there, or the move, a removal or the sync fails
fn set_aside(dir: &Path, suffix: &str) -> anyhow::Result<PathBuf> {
    let aside = aside_of(dir, suffix);
    if aside.exists() {
        anyhow::ensure!(
            suffix == "unchecked",
            "an earlier copy is set aside at {}: move it away for this one to be set aside",
            aside.display()
        );
        std::fs::remove_dir_all(&aside).with_context(|| format!("removing {}", aside.display()))?;
    }
    std::fs::rename(dir, &aside).with_context(|| format!("setting the copy aside to {}", aside.display()))?;
    let parent = dir.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::File::open(parent)?.sync_all().with_context(|| format!("syncing {}", parent.display()))?;
    Ok(aside)
}

#[cfg(test)]
mod tests;
