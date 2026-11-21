//! How a follower's replica is built as its copy is opened: by one replay of the whole copy, or from a daily
//! snapshot, by a pass over the copy before the snapshot's checkpoint that applies nothing
//! (`sequencer::replay::Before`), then a replay of the copy from the snapshot's state, its checkpoint first, as
//! the replica takes the stream's entries. A build from a snapshot that fails, for any reason the copy or the
//! snapshot gives, pages Warning `snapshot_start_refused`, and the replica is built by a whole replay instead,
//! which refuses the copy for itself if it is the copy that is at fault.

use std::cell::Cell;
use std::collections::{BTreeSet, VecDeque};
use std::thread::JoinHandle;

use anyhow::{Context, anyhow, ensure};
use engine_core::Engine;
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use tracing::{error, info, warn};

use crate::checkpoints::Takeover;
use crate::daily_snapshots::{DailySnapshot, Signed, open_signed};
use crate::log::{Checkpoint, CheckpointDomain, LogEntry, Manifest, Offset, ReplayWorkers, offset_of};
use crate::sequencer::replay::{Before, LiveReplay};
use crate::standby::chain::Chain;
use crate::standby::open_copy_replayed;
use crate::standby::replica::rows::RowsBefore;
use crate::standby::replica::{Follower, Records, recording};

/// What a build holds as it reads the copy: the pass toward a daily snapshot's checkpoint, until the copy
/// reaches it, then the replica.
enum Building {
    /// Taken while the pass becomes the replica.
    Before(Option<Box<Toward>>),
    Live(Box<LiveReplay>),
}

/// The pass toward the checkpoint at `at`, with the snapshot's engine, whose root a thread of its own computes
/// meanwhile, over every key.
struct Toward {
    pass: Before,
    at: Offset,
    checkpoint: Checkpoint,
    rooting: Rooting,
}

/// The thread computing the root of a snapshot's state, which gives the engine back.
struct Rooting(Option<JoinHandle<Engine>>);

impl Drop for Rooting {
    /// Joins the thread, so a build that ends before the snapshot's checkpoint leaves none running.
    fn drop(&mut self) {
        if let Some(rooting) = self.0.take() {
            // A thread that panicked computed nothing the build still needs.
            let _ = rooting.join();
        }
    }
}

impl Rooting {
    /// The snapshot's engine, holding its state tree, once the thread has computed its root.
    /// Error cases:
    /// - the computation panicked
    fn engine(mut self) -> anyhow::Result<Engine> {
        let rooting = self.0.take().expect("the thread is joined only here or on drop");
        rooting.join().map_err(|_| anyhow!("computing the root of the snapshot's state panicked"))
    }
}

impl Toward {
    /// Starts the pass over the copy at `dir` toward `snapshot`'s checkpoint, its seals checked ahead by
    /// `workers`, and the thread computing the root of the snapshot's state.
    /// Error cases:
    /// - the thread cannot start
    fn start(
        snapshot: DailySnapshot,
        (domain, signers): (CheckpointDomain, BTreeSet<AccountId>),
        keep: &[u64],
        (dir, workers): (&std::path::Path, ReplayWorkers),
    ) -> anyhow::Result<Self> {
        let DailySnapshot { offset, checkpoint, mut engine, .. } = snapshot;
        let mut pass = Before::new(domain, signers, &engine, keep);
        pass.check_seals_ahead(dir, workers);
        let rooting = std::thread::Builder::new()
            .name("snapshot-root".into())
            .spawn(move || {
                engine.state_root();
                engine
            })
            .context("starting the thread that computes the snapshot's root")?;
        Ok(Self { pass, at: offset, checkpoint, rooting: Rooting(Some(rooting)) })
    }
}

impl Building {
    /// Applies the copy's next entry: to the pass before the snapshot's checkpoint; at the checkpoint, the
    /// pass becomes the replica, recording what `records` names, and applies it; after it, to the replica.
    /// Error cases:
    /// - the entry fails the pass (`Before::entry`) or the replica (`LiveReplay::apply`)
    /// - the copy holds another entry at the snapshot's offset than its checkpoint, or the snapshot's state does
    ///   not stand where the pass ends (`Before::into_live`), or its root cannot be computed
    fn apply(&mut self, entry: &LogEntry, records: &Records) -> anyhow::Result<()> {
        let toward = match self {
            Building::Live(replica) => return replica.apply(entry),
            Building::Before(toward) => toward.as_mut().context("the pass failed to become the replica")?,
        };
        if toward.pass.next_offset(entry) != Some(toward.at) {
            return toward.pass.entry(entry);
        }
        ensure!(
            matches!(entry, LogEntry::Checkpoint(held) if *held == toward.checkpoint),
            "the copy holds another entry at {} than the snapshot's checkpoint",
            toward.at
        );
        let Building::Before(Some(toward)) = std::mem::replace(self, Building::Before(None)) else {
            unreachable!("the pass is held until it becomes the replica")
        };
        let Toward { pass, rooting, checkpoint, .. } = *toward;
        let engine = rooting.engine()?;
        // A replica from a snapshot answers no boundary check: the range a check covers may start before the
        // snapshot, whose state the replica did not verify, and the member cannot tell.
        let (stats, marks, _, rows) = records;
        let mut replica =
            recording(pass.into_live(engine, &checkpoint)?, &(stats.clone(), marks.clone(), None, rows.clone()));
        replica.apply(entry)?;
        *self = Building::Live(Box::new(replica));
        Ok(())
    }

    /// Keeps the posted head `(seq, link)` as one the build holds (`LiveReplay::pin`).
    fn pin(&mut self, seq: u64, link: B256) {
        match self {
            Building::Live(replica) => _ = replica.pin(seq, link),
            Building::Before(Some(toward)) => _ = toward.pass.pin(seq, link),
            Building::Before(None) => {}
        }
    }

    /// The replica the build ended with.
    /// Error cases:
    /// - the copy ended before the snapshot's checkpoint
    fn replica(self) -> anyhow::Result<LiveReplay> {
        match self {
            Building::Live(replica) => Ok(*replica),
            Building::Before(toward) => Err(anyhow!(
                "the copy ends before the snapshot's checkpoint at {}",
                toward.map_or_else(|| "its offset".to_owned(), |toward| toward.at.to_string())
            )),
        }
    }
}

impl Follower {
    /// Opens the copy, building its replica from `snapshot`, a daily snapshot signed over its state by one of
    /// `signers`, when one is given, and otherwise, or when that build fails, by one replay of the whole copy
    /// from `fresh` (see `open_pinned`), the replica keeping the links and checkpoints at `keep` and pinning
    /// `posted`. A build from a snapshot that fails pages Warning `snapshot_start_refused` before the whole
    /// replay, but for a failure of `observe`, which ends the open.
    /// Error cases:
    /// - as `open_with`
    pub(super) fn open_starting(
        copy: (&std::path::Path, u64, ReplayWorkers),
        (domain, enclave, signers): (CheckpointDomain, AccountId, BTreeSet<AccountId>),
        fresh: impl Fn() -> LiveReplay,
        records: &Records,
        (posted, keep): (Option<(u64, B256)>, &[u64]),
        snapshot: Option<Signed>,
        mut observe: impl FnMut(&LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let ids = (domain, enclave, signers.clone());
        if let Some(snapshot) = snapshot {
            let (seq, path, signer) = (snapshot.snapshot.offset.seq, snapshot.path.clone(), snapshot.signer);
            let observing_failed = Cell::new(false);
            let observing = |entry: &LogEntry| observe(entry).inspect_err(|_| observing_failed.set(true));
            let (dir, _, workers) = copy;
            let mut first = Some(snapshot.snapshot);
            let mut prepare = || -> anyhow::Result<(Building, bool)> {
                let snapshot = match first.take() {
                    Some(snapshot) => snapshot,
                    // A build after a cut reads the snapshot again, as it reads the copy again.
                    None => open_signed(&path, domain, &signers)?.snapshot,
                };
                let toward = Toward::start(snapshot, (domain, signers.clone()), keep, (dir, workers))?;
                Ok((Building::Before(Some(Box::new(toward))), false))
            };
            match Self::open_with(copy, ids.clone(), &mut prepare, records, posted, observing) {
                Ok(mut follower) => {
                    info!(seq, snapshot = %path.display(), %signer, "built the replica from a daily snapshot");
                    let (_, _, _, rows) = records;
                    follower.rows_before = rows.clone().and_then(|store| RowsBefore::start(dir, seq, store));
                    return Ok(follower);
                }
                Err(e) if observing_failed.get() => return Err(e),
                Err(e) => warn!(
                    alert = "snapshot_start_refused",
                    seq,
                    "the replica does not start from daily snapshot {}, so it replays the whole copy: {e:#}",
                    path.display()
                ),
            }
        }
        let mut prepare = || -> anyhow::Result<(Building, bool)> {
            let mut replica = fresh();
            replica.check_seals_ahead(copy.0, copy.2);
            Ok((Building::Live(Box::new(replica)), true))
        };
        Self::open_with(copy, ids, &mut prepare, records, posted, observe)
    }

    /// Opens the copy in `dir`, building its replica as `prepare` says, as it is read: from the start of the
    /// copy, or a daily snapshot, and whether that build is a whole replay. A tail after the copy's last
    /// checkpoint that no accepted signer sealed is copied aside to `<dir>.unaccepted-<seq>`, paged as Critical
    /// `copy_tail_unaccepted`, and cut back (`cut`), and the replica built again, from what `prepare` gives
    /// again.
    /// Error cases:
    /// - the copy cannot be opened, or does not replay (see `Replay::apply`), or ends before a snapshot's
    ///   checkpoint, or `observe` fails
    /// - the copy holds such a tail and no checkpoint before it, or the tail cannot be kept or cut
    fn open_with(
        (dir, segment_commands, _): (&std::path::Path, u64, ReplayWorkers),
        (domain, enclave, recorded): (CheckpointDomain, AccountId, BTreeSet<AccountId>),
        prepare: &mut dyn FnMut() -> anyhow::Result<(Building, bool)>,
        records: &Records,
        posted: Option<(u64, B256)>,
        mut observe: impl FnMut(&LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let (mut opened, mut previous) = (VecDeque::new(), None);
        let mut apply = |building: &mut Building, entry: &LogEntry| {
            observe(entry)?;
            building.apply(entry, records)?;
            let offset = offset_of(previous, entry)?;
            // A retried build reads the copy again from its start.
            if previous.is_some_and(|was| offset <= was) {
                opened.clear();
            }
            previous = Some(offset);
            opened.push_back((offset, entry.clone()));
            if opened.len() > crate::liveness::link::RECENT_ENTRIES {
                opened.pop_front();
            }
            if let (Some((seq, link)), LogEntry::Command(command)) = (posted, entry)
                && command.sequenced.seq == seq
            {
                building.pin(seq, link);
            }
            Ok(())
        };
        let chain = Chain::new(domain, enclave);
        let mut replays = 0;
        let mut build = |prepare: &mut dyn FnMut() -> anyhow::Result<(Building, bool)>| {
            let (building, whole) = prepare()?;
            replays += u64::from(whole);
            let building = Cell::new(Some(building));
            let fresh = || building.take().expect("a build reads the copy once");
            let (writer, copied, built) =
                open_copy_replayed(dir, segment_commands.max(1), chain.clone(), fresh, &mut apply)
                    .context("the copy does not replay")?;
            anyhow::Ok((writer, copied, built.replica()?))
        };
        // A tail no accepted signer sealed is not the operator's history to vouch for: a reorged
        // promotion's, or one a host or an archive forged. It is kept aside as evidence and cut back to
        // the last checkpoint, which an accepted signer's seals reach, and the stream refills it.
        let built = build(&mut *prepare)?;
        let (writer, mut copied, mut replica) = match built.2.unaccepted_from() {
            None => built,
            Some((seq, sealer)) => {
                let replica = &built.2;
                let checkpoint = replica.last_checkpoint().cloned().with_context(|| {
                    format!("the copy's commands from seq {seq} carry no seal by an accepted signer, and no checkpoint precedes them")
                })?;
                let last = replica.last_seq().unwrap_or(seq);
                drop(built);
                let kept = crate::standby::cut::keep_tail(dir, seq).context("keeping the copy's unaccepted tail")?;
                error!(
                    alert = "copy_tail_unaccepted",
                    key = %sealer.map_or_else(|| "none".into(), |key| key.to_string()),
                    seq,
                    "the copy's command at seq {seq} carries no seal by a key its replay accepts, and no \
                     checkpoint covers it or the commands after it to seq {last}: a promotion a reorg removed, or a \
                     tail an archive mirror or the host forged; they are kept at {} and the copy is cut back to its \
                     checkpoint at seq {}, for the stream to refill",
                    kept.display(),
                    checkpoint.seq
                );
                let signer = checkpoint.signer.parse().context("the checkpoint's signer")?;
                let to = Takeover { seq: checkpoint.seq, root: checkpoint.root, signer };
                crate::standby::cut::cut_marked(dir, &to).context("cutting the copy's unaccepted tail")?;
                build(&mut *prepare)?
            }
        };
        // The stream's entries are applied one at a time, each seal checked as it arrives, and each command
        // reaches the stats worker, copied once here, as the replica applies it.
        replica.stop_checking_seals_ahead();
        replica.warm_stats();
        // The stream may carry commands a recorded signer sealed that no checkpoint the copy holds names,
        // as a promoted epoch's before its first, which a cut, now or at an earlier open, dropped: the
        // stream's refill takes them, as a tail does.
        recorded.iter().for_each(|signer| copied.admit(*signer));
        let (published, manifest) = tokio::sync::watch::channel(Manifest::default());
        let mut writer = writer;
        writer.publish_on(published);
        let dir = dir.to_owned();
        let (signers, rows_before) = (recorded, None);
        Ok(Self { writer, copied, replica, link: None, opened, dir, manifest, signers, replays, rows_before })
    }
}

#[cfg(test)]
mod tests;
