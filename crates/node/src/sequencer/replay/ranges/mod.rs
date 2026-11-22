//! `engine replay --snapshots`: a log replayed in ranges at once, split at the checkpoints its daily
//! snapshots name, those of a checkpoint past the log's last, or one the log does not hold at the snapshot's
//! offset, left out. The first range starts from an empty engine at the log's start; every other starts
//! from a snapshot (`daily_snapshots::load`), its resting-order index rebuilt from the accounts' orders, at
//! the checkpoint the log holds at the snapshot's offset, which must be the snapshot's own. Each range
//! applies that checkpoint first, which computes the snapshot's root over every key and compares it with
//! the checkpoint's (a fault naming the snapshot when they differ), checks its signature as any checkpoint's,
//! and restarts the seal chain there; then it replays strictly, as a whole-log replay does, through the
//! checkpoint where the next range starts. The pass over the ranges then requires each to start from the
//! checkpoint the one before ended at and from the state it ended in, compared by the SHA-256 of the whole
//! state's encoding, which binds what the root leaves out too. A range from a snapshot that failed is
//! compared the same way before its own fault is reported, so a snapshot that differs from the state the
//! log reaches is blamed, not the fault it led its range to. So the ranges tile the log as one history, as
//! a whole-log replay applies it, and a snapshot's root check only fails a bad snapshot before its range
//! runs.
//!
//! What a sequential replay carries across that checkpoint besides the engine is either reset there (the
//! seal chain, the last command's unmatched approvals and, under a trusted signer, the unauthenticated
//! tail), or recomputed by the pass over the ranges' results in log order (`stitch`), which applies
//! nothing and reads nothing from a snapshot: a typed payout's checkpoint and a deferred approval that an
//! entry names before its range, and a custody finality activation's legacy credits (`Refs`), the counts,
//! the checkpoint signers and the activations. The ranges
//! fold neither the command prefix nor checkpoint accounting, which only a node's start and checkpoint work
//! read; an audit's verdict reads neither.

#[cfg(test)]
pub(crate) mod fixture;
mod stitch;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::ensure;
use engine_core::deposits::LegacyDeposit;
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};

use crate::checkpoints::SignerHistory;
use crate::daily_snapshots;
use crate::log::{self, Checkpoint, CheckpointDomain, CheckpointParent, LogEntry, Offset, SegmentedLog, Stop};
pub use crate::sequencer::replay::ranges::stitch::Audited;
use crate::sequencer::replay::roots::RootMismatch;
use crate::sequencer::replay::{Replay, Replayed};

/// What a range's replay leaves to the pass over the ranges: entries that name what came before it.
#[derive(Debug, Default)]
pub(crate) struct Refs {
    /// The seq of the checkpoint the range starts at, or none for the range at the log's start.
    pub(crate) start: Option<u64>,
    /// The checkpoints typed payouts named from before the start: their seqs and roots.
    pub(crate) payouts: Vec<(u64, B256)>,
    /// The deferred approvals withdrawal entries released that a checkpoint at or before the start deferred.
    pub(crate) deferred: Vec<(u64, AccountId, Fixed, u64)>,
    /// The legacy credits each custody finality activation lists, which a whole replay checks against the
    /// ledger the commands before custody finality built.
    pub(crate) ledgers: Vec<Vec<LegacyDeposit>>,
}

/// Whom a replay trusts, as `engine replay`'s flags say.
pub(crate) enum Trust {
    /// One pinned signer, or none, in a deployment the log records or the caller names
    /// (`replay_path_with_trust`).
    Pinned { domain: Option<CheckpointDomain>, signer: Option<AccountId> },
    /// The independently attested signers, one per signer epoch (`replay_path_with_attested`).
    Attested { domain: Option<CheckpointDomain>, attested: BTreeSet<AccountId> },
    /// The vault's finalized anchor and signer history (`replay_path_with_authorities`): no range starts
    /// after the anchor, so the checkpoints a recovery chains by their signed parents fall in the last.
    Custody { domain: CheckpointDomain, signer: AccountId, parent: CheckpointParent, authorities: SignerHistory },
}

/// A range of the log: where it starts, a daily snapshot or the log's start, and the offset of the
/// checkpoint where the next range starts, if one does.
struct Range {
    start: Option<(Offset, PathBuf)>,
    end: Option<Offset>,
}

/// What one range's replay gives the pass.
pub(super) struct RangeReplayed {
    /// The range's replay, with the engine it ends in only for the range that ends the log: the pass reads
    /// no other, so each other's is dropped as its range ends.
    pub(super) replayed: Replayed,
    pub(super) refs: Option<Refs>,
    /// Whether the range held the vault's finalized anchor (custody trust only).
    pub(super) anchored: bool,
    /// Where a range from a snapshot starts: its checkpoint and the state the range applies the log to.
    pub(super) start: Option<Boundary>,
    /// Where a range the log goes on past ends: the checkpoint the next range starts at, and the state the
    /// range ends in there.
    pub(super) end: Option<Boundary>,
}

/// A checkpoint where one range ends and the next starts, and the SHA-256 of the whole state's encoding
/// there (`daily_snapshots::state_digest`): the pass requires the two ranges to agree on both.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Boundary {
    pub(super) checkpoint: Checkpoint,
    pub(super) state: [u8; 32],
}

/// The context of a failed range from a daily snapshot: the snapshot and where it starts, so the pass can
/// check the start against where the range before ends before it reports the range's own fault.
#[derive(Debug)]
pub(super) struct FromSnapshot {
    pub(super) start: Boundary,
    pub(super) file: PathBuf,
}

impl std::fmt::Display for FromSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "from daily snapshot {}", self.file.display())
    }
}

impl Trust {
    /// A replay with this trust, its seals checked at the end of each run and its roots ahead.
    /// Error cases:
    /// - the custody trust's signer history is invalid (`Replay::set_authorities`)
    fn replay(&self) -> anyhow::Result<Replay> {
        let mut replay = match self {
            Trust::Pinned { domain, signer } => Replay::with_trust(*domain, *signer),
            Trust::Attested { domain, attested } => Replay::with_attested(*domain, attested.clone()),
            Trust::Custody { domain, signer, parent, authorities } => {
                let mut replay = Replay::with_recovery(Some(*domain), Some(*signer), *parent);
                replay.set_authorities(authorities.clone())?;
                replay
            }
        };
        replay.check_run_ends();
        replay.check_roots_ahead();
        Ok(replay)
    }

    /// The checks a whole-log replay with this trust runs once it finishes, for one range of it.
    /// Error cases:
    /// - as the trust's whole-log replay: a checkpoint signer other than the pinned one, or a command
    ///   tail no trusted signer sealed
    fn check(&self, replayed: &Replayed) -> anyhow::Result<()> {
        match self {
            Trust::Pinned { signer: None, .. } => {}
            Trust::Pinned { signer: Some(signer), .. } => {
                replayed.origin.require_audit(*signer)?;
                ensure!(
                    replayed.checkpoint_signers.iter().all(|named| named == signer),
                    "checkpoint signer differs from trusted signer"
                );
            }
            Trust::Attested { attested, .. } => replayed.origin.require_attested(attested)?,
            Trust::Custody { domain, signer, .. } => replayed.origin.require_recovery(*domain, *signer)?,
        }
        Ok(())
    }

    /// The seq after which no range starts: the vault's finalized anchor under custody trust.
    fn last_start(&self) -> Option<u64> {
        match self {
            Trust::Custody { parent, .. } if parent.has_checkpoint => Some(parent.seq),
            Trust::Custody { .. } => Some(0),
            _ => None,
        }
    }
}

/// How many ranges `engine replay --snapshots` replays at once when not told: one for every two physical
/// cores, since a range keeps about one and a half busy (its apply, read and root threads) and holds an
/// engine of its own, and no more than one for every two cores this process may use; at least one.
pub fn default_jobs() -> NonZeroUsize {
    let usable = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    NonZeroUsize::new(num_cpus::get_physical().min(usable) / 2).unwrap_or(NonZeroUsize::MIN)
}

/// `engine replay --snapshots`: replays the log directory at `path` in ranges, from the daily snapshots in
/// `snapshots`, `jobs` at once, under one pinned signer or none, as `replay_path_with_trust` replays a whole log.
/// Error cases:
/// - as `replay_ranges`
pub fn replay_ranges_with_trust(
    path: &Path,
    snapshots: &Path,
    domain: Option<CheckpointDomain>,
    signer: Option<AccountId>,
    jobs: NonZeroUsize,
) -> anyhow::Result<Audited> {
    replay_ranges(path, snapshots, Trust::Pinned { domain, signer }, jobs)
}

/// `replay_ranges_with_trust` under the independently attested signers, one per signer epoch, as
/// `replay_path_with_attested` replays a whole log.
/// Error cases:
/// - as `replay_ranges`
pub fn replay_ranges_with_attested(
    path: &Path,
    snapshots: &Path,
    domain: Option<CheckpointDomain>,
    attested: BTreeSet<AccountId>,
    jobs: NonZeroUsize,
) -> anyhow::Result<Audited> {
    replay_ranges(path, snapshots, Trust::Attested { domain, attested }, jobs)
}

/// Replays the log directory at `path` in ranges at once, `jobs` at a time, split at the checkpoints of
/// the daily snapshots in `snapshots` that the log holds, as the module says, under `trust`. A fault
/// fails the replay as a whole-log replay would fail it: the earliest range's, or the pass's at an entry
/// before any later range's.
/// Error cases:
/// - `path` is not a log directory, its last checkpoint cannot be read (`log::last_checkpoint`), or
///   `snapshots` cannot be listed
/// - a range fails (`replay_range`), or the pass does (`stitch`)
/// - the log holds no command or no checkpoint, or, under custody trust, omits the finalized anchor
pub(crate) fn replay_ranges(
    path: &Path,
    snapshots: &Path,
    trust: Trust,
    jobs: NonZeroUsize,
) -> anyhow::Result<Audited> {
    replay_ranges_from(&Engine::default, path, snapshots, trust, jobs)
}

/// `replay_ranges` of a log whose first range starts from `base`'s engine, where a whole log's starts from
/// an empty one: a log a sequencer wrote after it took over a state no log holds (`Replay::seeded`), in a
/// test; `Engine::default` otherwise.
/// Error cases:
/// - as `replay_ranges`
pub(crate) fn replay_ranges_from(
    base: &(dyn Fn() -> Engine + Sync),
    path: &Path,
    snapshots: &Path,
    trust: Trust,
    jobs: NonZeroUsize,
) -> anyhow::Result<Audited> {
    ensure!(path.is_dir(), "{} is not a log directory: a replay in ranges reads a segmented log", path.display());
    // A snapshot past the log's last checkpoint, as a node's newest is past an archive of its completed
    // segments, starts no range; nor does one of a checkpoint the log does not hold at its offset, as one a
    // node wrote in a tail it later set aside.
    let held = log::last_checkpoint(path)?.map(|checkpoint| checkpoint.seq);
    let bound = trust.last_start().map_or(held, |last| held.map(|held| held.min(last)));
    let candidates: Vec<(Offset, PathBuf)> = daily_snapshots::list(snapshots)?
        .into_iter()
        .filter(|(offset, _)| bound.is_some_and(|bound| offset.seq <= bound))
        .collect();
    let usable = run(&candidates, jobs, |(offset, file), _| holds(path, *offset, file));
    let (mut starts, mut skipped): (Vec<(Offset, PathBuf)>, Vec<PathBuf>) = (Vec::new(), Vec::new());
    for (usable, (offset, file)) in usable.into_iter().zip(candidates) {
        if !usable? {
            skipped.push(file);
        } else if starts.last().is_none_or(|(last, _)| offset.seq > last.seq) {
            starts.push((offset, file));
        }
    }
    let ends: Vec<Option<Offset>> = starts.iter().map(|(offset, _)| Some(*offset)).chain([None]).collect();
    let ranges: Vec<Range> = [None]
        .into_iter()
        .chain(starts.into_iter().map(Some))
        .zip(ends)
        .map(|(start, end)| Range { start, end })
        .collect();
    let results = run(&ranges, jobs, |range, last| replay_range(path, (range, last), &trust, base));
    let replayed = Audited { skipped, ..stitch::stitch(results, ranges.len(), &trust)? };
    ensure!(replayed.commands > 0, "{} holds no command", path.display());
    ensure!(replayed.checkpoints > 0, "{} holds no checkpoint, so nothing in it is verified", path.display());
    Ok(replayed)
}

/// Whether the log directory at `path` holds at `offset` the checkpoint the daily snapshot at `file` names;
/// a file whose checkpoint does not read names none.
/// Error cases:
/// - the log cannot be read up to `offset` (`SegmentedLog::read_span_ahead`)
fn holds(path: &Path, offset: Offset, file: &Path) -> anyhow::Result<bool> {
    let Ok(named) = daily_snapshots::checkpoint_of(file) else { return Ok(false) };
    Ok(matches!(daily_snapshots::entry_at(path, offset)?, Some(LogEntry::Checkpoint(held)) if held == named))
}

/// Runs `replay` over each of `ranges`, told whether it is the last, on `jobs` threads, each taking the next
/// not yet taken, and gives their results in order, ending at the first that failed: once one fails, no
/// thread takes a later one, whose result the earlier fault makes moot.
fn run<R: Sync, T: Send>(
    ranges: &[R],
    jobs: NonZeroUsize,
    replay: impl Fn(&R, bool) -> anyhow::Result<T> + Sync,
) -> Vec<anyhow::Result<T>> {
    let next = AtomicUsize::new(0);
    let failed = AtomicUsize::new(usize::MAX);
    let results: Vec<Mutex<Option<anyhow::Result<T>>>> = ranges.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..jobs.get().min(ranges.len()) {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    if at >= ranges.len() || at > failed.load(Ordering::Relaxed) {
                        return;
                    }
                    let result = replay(&ranges[at], at + 1 == ranges.len());
                    if result.is_err() {
                        failed.fetch_min(at, Ordering::Relaxed);
                    }
                    *results[at].lock().expect("no thread panics holding a result") = Some(result);
                }
            });
        }
    });
    let mut ordered = Vec::new();
    for result in
        results.into_iter().map_while(|result| result.into_inner().expect("no thread panics holding a result"))
    {
        let failed = result.is_err();
        ordered.push(result);
        if failed {
            break;
        }
    }
    ordered
}

/// Replays `range` of the log at `path` under `trust`, `last` when it ends the log: from its snapshot's
/// checkpoint, applied first over the snapshot's state, or from the log's start, over `base`'s engine,
/// through the checkpoint at its end, as the module says. A range from a snapshot that fails once the
/// snapshot has loaded carries `FromSnapshot` in its error.
/// Error cases:
/// - the snapshot does not load (`daily_snapshots::load`), or the log holds no checkpoint at its offset,
///   or another than the snapshot's
/// - the log ends before the range's end checkpoint
/// - an entry fails replay (`Replay::apply`), or the range fails its trust's checks (`Trust::check`)
fn replay_range(
    path: &Path,
    (range, last): (&Range, bool),
    trust: &Trust,
    base: &(dyn Fn() -> Engine + Sync),
) -> anyhow::Result<RangeReplayed> {
    let mut started = None;
    replay_span(path, (range, last), trust, base, &mut started).map_err(|failed| match (started, &range.start) {
        (Some(start), Some((_, file))) => failed.context(FromSnapshot { start, file: file.clone() }),
        _ => failed,
    })
}

/// `replay_range`, setting `started` to the range's start once its snapshot has loaded.
/// Error cases:
/// - as `replay_range`
fn replay_span(
    path: &Path,
    (range, last): (&Range, bool),
    trust: &Trust,
    base: &(dyn Fn() -> Engine + Sync),
    started: &mut Option<Boundary>,
) -> anyhow::Result<RangeReplayed> {
    let mut replay = trust.replay()?;
    replay.range = Some(Refs::default());
    let (from, mut first, start) = match &range.start {
        None => {
            replay.engine = base();
            (0, None, None)
        }
        Some((offset, file)) => {
            let snapshot = daily_snapshots::load(file)?;
            let state = daily_snapshots::state_digest(&snapshot.engine)?;
            replay.engine = snapshot.engine;
            // A snapshot's commands all carry their events (`daily_snapshots::load` requires V27).
            replay.logs_events = true;
            replay.range = Some(Refs { start: Some(offset.seq), ..Refs::default() });
            let start = Boundary { checkpoint: snapshot.checkpoint.clone(), state };
            *started = Some(start.clone());
            (offset.seq, Some((*offset, snapshot.checkpoint, file.clone())), Some(start))
        }
    };
    let custody_anchor = match trust {
        Trust::Custody { parent, .. } if parent.has_checkpoint => Some(*parent),
        _ => None,
    };
    let (mut at, mut ended, mut anchored) = (None, None, false);
    let read = SegmentedLog::read_span_ahead(path, from, |entry| {
        let offset = log::offset_of(at, &entry)?;
        at = Some(offset);
        if let Some((start, checkpoint, file)) = &first {
            if offset != *start {
                return Ok(());
            }
            let held = matches!(&entry, LogEntry::Checkpoint(held) if held == checkpoint);
            ensure!(held, "the log holds another entry at {start} than daily snapshot {}'s checkpoint", file.display());
            first = None;
        }
        if let (Some(parent), LogEntry::Checkpoint(cp)) = (custody_anchor, &entry)
            && cp.seq == parent.seq
        {
            ensure!(cp.root.parse::<B256>()? == parent.root, "public log differs from the finalized vault anchor");
            anchored = true;
        }
        replay.apply(&entry)?;
        if range.end == Some(offset) {
            ended = Some(entry);
            return Err(Stop.into());
        }
        Ok(())
    });
    if let Err(failed) = replay.settled(read) {
        // The range before it verified the log's root at this checkpoint, so the snapshot's state fails it.
        return Err(match (&range.start, failed.downcast_ref::<RootMismatch>()) {
            (Some((start, file)), Some(mismatch)) if mismatch.seq == start.seq => failed.context(format!(
                "daily snapshot {}'s state does not match the root of its checkpoint at {start}, which the log holds",
                file.display()
            )),
            _ => failed,
        });
    }
    if let Some((start, _, file)) = &first {
        anyhow::bail!("the log holds no checkpoint at {start}, which daily snapshot {} names", file.display());
    }
    let end = match (range.end, ended) {
        (None, _) => None,
        (Some(_), Some(LogEntry::Checkpoint(checkpoint))) => {
            Some(Boundary { checkpoint, state: daily_snapshots::state_digest(&replay.engine)? })
        }
        (Some(end), Some(_)) => anyhow::bail!("the log holds no checkpoint at {end}, where the next range starts"),
        (Some(end), None) => anyhow::bail!("the log ends before the checkpoint at {end} where the next range starts"),
    };
    if last && matches!(trust, Trust::Custody { .. }) {
        replay.ensure_superseded_continued()?;
    }
    let refs = replay.range.take();
    let mut replayed = replay.finish();
    trust.check(&replayed)?;
    if end.is_some() {
        drop(std::mem::take(&mut replayed.engine));
    }
    Ok(RangeReplayed { replayed, refs, anchored, start, end })
}
