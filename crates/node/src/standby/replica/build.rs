//! How a follower's replica is built as its copy is opened: by one replay of the whole copy, or from a daily
//! snapshot, whose engine and the replay state it carries stand for the copy through its checkpoint
//! (`LiveReplay::resume`). A build from a snapshot reads the copy from the command that checkpoint covers, which a
//! resumed stream repeats first, requires the checkpoint at the snapshot's offset, and replays the entries after
//! it, so the copy's segments before it need not be on this host. A build from a snapshot that fails, for any
//! reason the copy or the snapshot gives, pages Warning `snapshot_start_refused`, and the replica is built by a
//! whole replay instead, which refuses the copy for itself if it is the copy that is at fault.

use std::cell::Cell;
use std::collections::{BTreeSet, VecDeque};

use anyhow::{Context, anyhow, ensure};
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use tracing::{error, info, warn};

use crate::checkpoints::Takeover;
use crate::daily_snapshots::{DailySnapshot, Signed, open_signed};
use crate::log::{Checkpoint, CheckpointDomain, LogEntry, Manifest, Offset, ReplayWorkers, offset_of};
use crate::sequencer::replay::LiveReplay;
use crate::standby::chain::Chain;
use crate::standby::open_copy_replayed;
use crate::standby::replica::rows::RowsBefore;
use crate::standby::replica::{Follower, Records, recording_from};

/// What a build holds as it reads the copy: the replica, and for one gone on from a daily snapshot, the
/// snapshot's checkpoint, with its offset, until the copy reaches it.
struct Building {
    replica: LiveReplay,
    /// The entries before it the snapshot stands for: the replica stands just past it.
    toward: Option<(Offset, Checkpoint)>,
}

impl Building {
    /// A replica of the copy in `dir`, in `domain` under `signers`, going on from `snapshot`, keeping the links and
    /// checkpoints at `keep`, recording what `records` names, its seals checked
    /// ahead by `workers`, and holding the posted head `posted` when the links it carries include it; with the
    /// offset the copy is read from, that of the command the snapshot's checkpoint covers. Its dashboard record
    /// goes on from the one the snapshot carries.
    /// Error cases:
    /// - the snapshot does not stand for its engine and checkpoint, or its root is not its checkpoint's
    ///   (`LiveReplay::resume`)
    fn resume(
        snapshot: DailySnapshot,
        (domain, signers): (CheckpointDomain, BTreeSet<AccountId>),
        records: &Records,
        (keep, posted): (&[u64], Option<(u64, B256)>),
        (dir, workers): (&std::path::Path, ReplayWorkers),
    ) -> anyhow::Result<(Self, Offset)> {
        let toward = (snapshot.offset, snapshot.checkpoint.clone());
        let record = crate::daily_snapshots::History::read(snapshot.history.as_deref()).stats;
        let resumed = LiveReplay::resume(domain, signers, snapshot, keep)?;
        // A replica from a snapshot answers a boundary check only for a range that starts after the snapshot's
        // checkpoint, since it applied no command before it (`Checked::from`). Its dashboard record goes on from the
        // one the snapshot carries.
        let mut replica = recording_from(resumed, records, record);
        replica.check_seals_ahead(dir, workers);
        if let Some((seq, link)) = posted {
            replica.pin(seq, link);
        }
        let from = Offset::start_of(toward.0.seq);
        Ok((Self { replica, toward: Some(toward) }, from))
    }

    /// Applies the copy's entry at `offset`: before a daily snapshot's checkpoint, nothing; at it, nothing, as the
    /// replica stands past it; after it, the replica applies it.
    /// Error cases:
    /// - the copy holds another entry at the snapshot's offset than its checkpoint
    /// - the entry fails the replica (`LiveReplay::apply`)
    fn apply(&mut self, offset: Offset, entry: &LogEntry) -> anyhow::Result<()> {
        match &self.toward {
            None => self.replica.apply(entry),
            Some((at, _)) if offset < *at => Ok(()),
            Some((at, checkpoint)) => {
                ensure!(
                    offset == *at && matches!(entry, LogEntry::Checkpoint(held) if held == checkpoint),
                    "the copy holds another entry at {at} than the snapshot's checkpoint"
                );
                self.toward = None;
                Ok(())
            }
        }
    }

    /// The replica the build ended with.
    /// Error cases:
    /// - the copy ended before the snapshot's checkpoint
    fn replica(self) -> anyhow::Result<LiveReplay> {
        match self.toward {
            None => Ok(self.replica),
            Some((at, _)) => Err(anyhow!("the copy ends before the snapshot's checkpoint at {at}")),
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
            let mut prepare = || -> anyhow::Result<(Building, Option<Offset>)> {
                let snapshot = match first.take() {
                    Some(snapshot) => snapshot,
                    // A build after a cut reads the snapshot again, as it reads the copy again.
                    None => open_signed(&path, domain, &signers)?.snapshot,
                };
                let trust = (domain, signers.clone());
                let (building, from) = Building::resume(snapshot, trust, records, (keep, posted), (dir, workers))?;
                Ok((building, Some(from)))
            };
            match Self::open_with(copy, ids.clone(), &mut prepare, posted, observing) {
                Ok(mut follower) => {
                    info!(seq, snapshot = %path.display(), %signer, "built the replica from a daily snapshot");
                    let (_, _, _, rows) = records;
                    let now = follower.replica().engine().now();
                    follower.rows_before = rows.clone().and_then(|store| RowsBefore::start(dir, (seq, now), store));
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
        let mut prepare = || -> anyhow::Result<(Building, Option<Offset>)> {
            let mut replica = fresh();
            replica.check_seals_ahead(copy.0, copy.2);
            Ok((Building { replica, toward: None }, None))
        };
        Self::open_with(copy, ids, &mut prepare, posted, observe)
    }

    /// Opens the copy in `dir`, building its replica as `prepare` says, as it is read: from the start of the
    /// copy, a whole replay, or from the offset `prepare` gives, a daily snapshot's. A tail after the copy's last
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
        prepare: &mut dyn FnMut() -> anyhow::Result<(Building, Option<Offset>)>,
        posted: Option<(u64, B256)>,
        mut observe: impl FnMut(&LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let (mut opened, mut previous) = (VecDeque::new(), None);
        let mut apply = |building: &mut Building, entry: &LogEntry| {
            observe(entry)?;
            let offset = offset_of(previous, entry)?;
            building.apply(offset, entry)?;
            // A retried build reads the copy again from where it starts.
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
                building.replica.pin(seq, link);
            }
            Ok(())
        };
        let chain = Chain::new(domain, enclave);
        let mut replays = 0;
        let mut build = |prepare: &mut dyn FnMut() -> anyhow::Result<(Building, Option<Offset>)>| {
            let (building, from) = prepare()?;
            replays += u64::from(from.is_none());
            let building = Cell::new(Some(building));
            let fresh = || building.take().expect("a build reads the copy once");
            let (writer, copied, built) =
                open_copy_replayed(dir, segment_commands.max(1), chain.clone(), from, fresh, &mut apply)
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
