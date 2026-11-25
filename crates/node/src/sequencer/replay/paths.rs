//! The public replay entry points: whole logs and signed audit paths.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, ensure};
use tracing::warn;

use engine_core::types::AccountId;

use crate::daily_snapshots::Signed;
use crate::log::{self, CheckpointDomain, LogEntry, ReplayWorkers, SealChecks, SegmentedLog};
use crate::sequencer::replay::resumed::SnapshotRefused;
use crate::sequencer::replay::{Replay, Replayed};

#[cfg(test)]
impl Replay {
    /// A replay pinned to `signer` in `domain`, as `with_trust` builds one, that starts from `engine`, a
    /// state no log holds (`Sequencer::take_over`), where a whole log's replay starts from an empty one.
    pub(in crate::sequencer) fn seeded(
        engine: engine_core::Engine,
        domain: Option<CheckpointDomain>,
        signer: AccountId,
    ) -> Self {
        let mut replay = Self::with_trust(domain, Some(signer));
        replay.engine = engine;
        replay
    }
}

/// Replays with an independently trusted signer for withdrawal completion evidence, its command seals
/// checked at the end of each run (`SealChecks::RunEnds`).
/// Error cases:
/// - any entry, signature, or authorization fails replay
pub fn replay_with_signer(
    entries: &[LogEntry],
    domain: CheckpointDomain,
    signer: AccountId,
) -> anyhow::Result<Replayed> {
    let mut replay = Replay::with_trust(Some(domain), Some(signer));
    replay.check_run_ends();
    replay.check_roots_ahead();
    let applied = entries.iter().try_for_each(|entry| replay.apply(entry));
    replay.settled(applied)?;
    let replayed = replay.finish();
    replayed.origin.require_audit(signer)?;
    ensure!(
        replayed.checkpoint_signers.iter().all(|named| *named == signer),
        "checkpoint signer differs from trusted signer"
    );
    Ok(replayed)
}

/// Replays the log at `path`, a single log file or a directory of segments with their manifest, as
/// it reads it, for `engine replay`, its command seals checked at the end of each run
/// (`SealChecks::RunEnds`). A log that holds no checkpoint verifies nothing, so it fails.
/// Error cases:
/// - nothing is at `path`, or it cannot be read
/// - the log fails its manifest check (see `SegmentedLog::open`) or replay (see `Replay::apply`)
/// - the log holds no command, or no checkpoint
pub fn replay_path(path: &Path) -> anyhow::Result<Replayed> {
    replay_path_in_domain(path, None)
}

/// Replays signatures in an explicit deployment, including logs that predate domain recording.
/// Error cases:
/// - the log fails replay or conflicts with `domain` (see `replay_path`)
pub fn replay_path_in_domain(path: &Path, domain: Option<CheckpointDomain>) -> anyhow::Result<Replayed> {
    replay_path_with_trust(path, domain, None, SealChecks::RunEnds)
}

/// Replays a published log with explicit authority for authenticating withdrawal releases, its command
/// seals checked as `seals` says.
/// Error cases:
/// - log, root, signature, domain or trusted signer validation fails
pub fn replay_path_with_trust(
    path: &Path,
    domain: Option<CheckpointDomain>,
    signer: Option<AccountId>,
    seals: SealChecks,
) -> anyhow::Result<Replayed> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    let mut replay = Replay::with_trust(domain, signer);
    replay.check_ahead(path, seals);
    let read = if found.is_dir() {
        SegmentedLog::open_ahead(path, |entry| replay.apply(&entry)).map(drop)
    } else {
        log::read_file_ahead(path, |entry| replay.apply(&entry))
    };
    replay.settled(read)?;
    let replayed = replay.finish();
    if let Some(signer) = signer {
        replayed.origin.require_audit(signer)?;
        ensure!(
            replayed.checkpoint_signers.iter().all(|named| *named == signer),
            "checkpoint signer differs from trusted signer"
        );
    }
    ensure!(replayed.commands > 0, "{} holds no command", path.display());
    ensure!(replayed.checkpoints > 0, "{} holds no checkpoint, so nothing in it is verified", path.display());
    Ok(replayed)
}

/// Replays the log directory at `path`, written by a sequencer that took over `engine`, a state no log
/// holds, from that state, as `replay_path_with_trust` replays a whole log under `signer`: every
/// command's events, every checkpoint's root and signature, and the command seals, checked as `seals`
/// says.
/// Error cases:
/// - as `replay_path_with_trust`
#[cfg(test)]
pub(crate) fn replay_path_from(
    engine: engine_core::Engine,
    path: &Path,
    signer: AccountId,
    seals: SealChecks,
) -> anyhow::Result<Replayed> {
    replay_path_from_observing(engine, path, signer, seals, |_| {})
}

/// `replay_path_from`, with `observe` seeing each entry before it is applied.
/// Error cases:
/// - as `replay_path_with_trust`
#[cfg(test)]
pub(crate) fn replay_path_from_observing(
    engine: engine_core::Engine,
    path: &Path,
    signer: AccountId,
    seals: SealChecks,
    mut observe: impl FnMut(&LogEntry),
) -> anyhow::Result<Replayed> {
    let mut replay = Replay::seeded(engine, None, signer);
    replay.check_ahead(path, seals);
    let read = SegmentedLog::open_ahead(path, |entry| {
        observe(&entry);
        replay.apply(&entry)
    });
    replay.settled(read)?;
    let replayed = replay.finish();
    replayed.origin.require_audit(signer)?;
    ensure!(
        replayed.checkpoint_signers.iter().all(|named| *named == signer),
        "checkpoint signer differs from trusted signer"
    );
    ensure!(replayed.commands > 0, "{} holds no command", path.display());
    ensure!(replayed.checkpoints > 0, "{} holds no checkpoint, so nothing in it is verified", path.display());
    Ok(replayed)
}

/// Publishes into `snapshots`, signed by `signing`, the daily snapshot a node writes of the log in `dir` at the
/// checkpoint `pick` names among its checkpoints of a state with custody finality, given how many there are: the
/// engine and the replay state a whole replay under `signing`'s key holds there, with no history anchors. Its
/// listing.
#[cfg(test)]
pub(crate) fn publish_daily_snapshot(
    dir: &Path,
    snapshots: &Path,
    signing: crate::daily_snapshots::Signing,
    pick: impl FnOnce(usize) -> usize,
) -> crate::daily_snapshots::Listed {
    use crate::snapshot::sealing::Encoding;
    let signer = AccountId(signing.key.address());
    let mut replay = Replay::with_trust(Some(signing.domain), Some(signer));
    let mut held = Vec::new();
    SegmentedLog::open(dir, |entry| {
        replay.apply(&entry)?;
        if let LogEntry::Checkpoint(checkpoint) = &entry
            && replay.engine.deposit_finality_enabled()
        {
            let (engine, carried) =
                (serde_json::to_string(&replay.engine)?, serde_json::to_string(&replay.carried()?)?);
            let offset = replay.last_offset.expect("the checkpoint's offset");
            held.push((offset, checkpoint.clone(), replay.engine.now(), engine, carried));
        }
        Ok(())
    })
    .expect("the log replays");
    let (offset, checkpoint, ts, engine, carried) = held.swap_remove(pick(held.len()));
    let (engine, carried, history) =
        (Encoding::Encoded(engine), Encoding::Encoded(carried), crate::daily_snapshots::Pending::none());
    let job = crate::daily_snapshots::Job { offset, checkpoint, ts, engine, carried, history, signing };
    crate::daily_snapshots::publish_now(snapshots, job).expect("the snapshot publishes")
}

/// Removes from the log directory `dir` every segment file before the one holding seq `seq`, leaving the manifest
/// listing them, as a host that keeps only the segments from a daily snapshot's on does. How many it removed.
#[cfg(test)]
pub(crate) fn remove_segments_before(dir: &Path, seq: u64) -> usize {
    let listed = crate::log::Manifest::load(dir).expect("the manifest").segments;
    let held = listed.iter().rposition(|segment| segment.first_seq <= seq).expect("a segment holds the seq");
    listed[..held].iter().for_each(|segment| fs::remove_file(dir.join(&segment.file)).expect("a segment file"));
    held
}

/// Replays a published log whose checkpoint signer changes at signer epochs the vault records,
/// given an independently verified attestation for each signer: every checkpoint and every typed
/// withdrawal must be one of `attested`, unlike `replay_path_with_trust`'s single pinned signer. The
/// command seals are checked as `seals` says.
/// Error cases:
/// - log, root, signature, or domain validation fails, or a checkpoint or typed withdrawal names
///   no attested signer
pub fn replay_path_with_attested(
    path: &Path,
    domain: Option<CheckpointDomain>,
    attested: BTreeSet<AccountId>,
    seals: SealChecks,
) -> anyhow::Result<Replayed> {
    replay_attested(path, Replay::with_attested(domain, attested.clone()), attested, seals)
}

/// Replays a copy of another node's log, which an archive or a predecessor supplied, as
/// `replay_path_with_attested` does in `domain`, and every command must carry a seal by one of
/// `attested` (`Replay::copy`, and `require_attested` for the tail no checkpoint covers): a copy that
/// dropped or replaced a seal is refused, so a copy that replays commits to its writer's cumulative
/// command prefix. Every seal is checked in full, ahead of the replay by `workers`, since the prefix
/// reads each seal's bytes.
/// Error cases:
/// - as `replay_path_with_attested`, or a command carries no seal by one of `attested`
pub(crate) fn replay_copy(
    path: &Path,
    domain: CheckpointDomain,
    attested: BTreeSet<AccountId>,
    workers: ReplayWorkers,
) -> anyhow::Result<Replayed> {
    replay_attested(path, Replay::copy(domain, attested.clone()), attested, SealChecks::Every(workers))
}

/// `replay_copy` of the copy in the log directory at `path`, going on from `start`, a daily snapshot whose
/// checkpoint the copy holds, when one is given: the copy is read only from that checkpoint on
/// (`Replay::replay_after`), so its segments before it need not be on this host, and what follows is checked as
/// `replay_copy` checks a whole copy. A snapshot the replay cannot go on from at all (`SnapshotRefused`) is paged
/// as Warning `snapshot_start_refused`, and the whole copy replays instead. With the seq of the snapshot's
/// checkpoint the replay went on from, or none for a replay of the whole copy.
/// Error cases:
/// - as `replay_copy`, or the copy holds another entry than the snapshot's checkpoint at its offset
pub(crate) fn replay_copy_from(
    start: Option<Signed>,
    path: &Path,
    domain: CheckpointDomain,
    attested: BTreeSet<AccountId>,
    workers: ReplayWorkers,
) -> anyhow::Result<(Replayed, Option<u64>)> {
    let Some(start) = start else {
        return replay_copy(path, domain, attested, workers).map(|replayed| (replayed, None));
    };
    let (file, seq) = (start.path.clone(), start.snapshot.offset.seq);
    let replay = Replay::copy(domain, attested.clone());
    match replay.replay_after(start, path, SealChecks::Every(workers), |_| Ok(())) {
        Ok((replay, _)) => Ok((attested_end(replay.finish(), path, &attested)?, Some(seq))),
        Err(refused) if refused.is::<SnapshotRefused>() => {
            warn!(
                alert = "snapshot_start_refused",
                seq,
                "the replay of the copy does not go on from daily snapshot {}: {refused:#}; the whole copy replays",
                file.display()
            );
            replay_copy(path, domain, attested, workers).map(|replayed| (replayed, None))
        }
        Err(refused) => Err(refused),
    }
}

/// Runs `replay`, an attested replay of `attested`, over the log at `path`, its command seals checked
/// as `seals` says.
/// Error cases:
/// - as `replay_path_with_attested`
fn replay_attested(
    path: &Path,
    mut replay: Replay,
    attested_set: BTreeSet<AccountId>,
    seals: SealChecks,
) -> anyhow::Result<Replayed> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    replay.check_ahead(path, seals);
    let read = if found.is_dir() {
        SegmentedLog::open_ahead(path, |entry| replay.apply(&entry)).map(drop)
    } else {
        log::read_file_ahead(path, |entry| replay.apply(&entry))
    };
    replay.settled(read)?;
    attested_end(replay.finish(), path, &attested_set)
}

/// `replayed`, an attested replay of `attested` over the log at `path`, once it holds a command and a
/// checkpoint, and no command after its last checkpoint lacks a seal by one of `attested`.
/// Error cases:
/// - it holds no command or no checkpoint, or its tail is not sealed by one of `attested` (`require_attested`)
fn attested_end(replayed: Replayed, path: &Path, attested: &BTreeSet<AccountId>) -> anyhow::Result<Replayed> {
    ensure!(replayed.commands > 0, "{} holds no command", path.display());
    ensure!(replayed.checkpoints > 0, "{} holds no checkpoint, so nothing in it is verified", path.display());
    // No attested key authenticated a command tail after the last checkpoint: an unsealed
    // append must not pass as verified.
    replayed.origin.require_attested(attested)?;
    Ok(replayed)
}

/// Replays one exact finalized prefix with independently authenticated historical authorities, its
/// command seals checked as `seals` says.
/// Error cases:
/// - log, root, signature, authority, parent continuity, or finalized anchor validation fails
pub(crate) fn replay_path_with_authorities(
    path: &Path,
    domain: CheckpointDomain,
    signer: AccountId,
    parent: crate::log::CheckpointParent,
    authorities: crate::checkpoints::SignerHistory,
    seals: SealChecks,
) -> anyhow::Result<Replayed> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    let mut replay = Replay::with_recovery(Some(domain), Some(signer), parent);
    replay.set_authorities(authorities)?;
    replay.check_ahead(path, seals);
    let mut anchored = !parent.has_checkpoint;
    let mut visit = |entry: LogEntry| {
        if let LogEntry::Checkpoint(cp) = &entry
            && parent.has_checkpoint
            && cp.seq == parent.seq
        {
            ensure!(
                cp.root.parse::<engine_core::eip712::B256>()? == parent.root,
                "public log differs from the finalized vault anchor"
            );
            anchored = true;
        }
        replay.apply(&entry)
    };
    let read = if found.is_dir() {
        SegmentedLog::open_ahead(path, &mut visit).map(drop)
    } else {
        log::read_file_ahead(path, &mut visit)
    };
    replay.settled(read)?;
    ensure!(anchored, "public log is shorter than or omits the finalized vault anchor");
    replay.ensure_superseded_continued()?;
    let replayed = replay.finish();
    replayed.origin.require_recovery(domain, signer)?;
    ensure!(replayed.commands > 0 && replayed.checkpoints > 0, "public log has no verified command/checkpoint");
    Ok(replayed)
}

#[cfg(test)]
mod tests;
