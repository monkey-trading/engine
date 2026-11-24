//! A replay of the log on this host from a daily snapshot's checkpoint on: the snapshot's engine and the replay
//! state it carries stand for every entry before the checkpoint, which is not read (`Replay::resume`), and the
//! entries after it are read and replayed as a whole replay replays them.

use anyhow::{Context, ensure};

use crate::daily_snapshots::{DailySnapshot, Signed};
use crate::log::{LogEntry, SealChecks, SegmentedLog};
use crate::sequencer::replay::Replay;

impl Replay {
    /// This replay, set up with its trust, going on from `signed`'s checkpoint, then reading the log in `dir`
    /// from the entry after it to its end, each entry handed to `observe` before the replay applies it, its
    /// seals and roots checked as `seals` says (`check_ahead`) and its read settled (`settled`). The segments
    /// before the one holding the checkpoint are not read, and need not be on this host.
    /// Error cases:
    /// - the snapshot carries no replay state, or it does not stand for the snapshot's engine and checkpoint, or
    ///   the engine's root is not the checkpoint's (`resume`)
    /// - the log holds another entry at the snapshot's offset than its checkpoint, or ends before it
    /// - the log cannot be read from there (`SegmentedLog::open_ahead_from`), an entry fails replay, or `observe`
    ///   fails
    pub(in crate::sequencer) fn replay_after(
        self,
        signed: Signed,
        dir: &std::path::Path,
        seals: SealChecks,
        mut observe: impl FnMut(&LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<(Self, SegmentedLog)> {
        let DailySnapshot { offset, checkpoint, engine, carried, .. } = signed.snapshot;
        let carried = carried.context("the daily snapshot carries no replay state")?;
        let mut replay = self.resume(engine, carried, checkpoint.clone(), offset)?;
        replay.check_ahead(dir, seals);
        let mut at_checkpoint = None;
        let read = SegmentedLog::open_ahead_from(dir, offset, |entry| {
            if at_checkpoint.is_none() {
                let held = matches!(&entry, LogEntry::Checkpoint(held) if *held == checkpoint);
                at_checkpoint = Some(held);
                ensure!(held, "the log holds another entry at {offset} than the daily snapshot's checkpoint");
                return Ok(());
            }
            observe(&entry)?;
            replay.apply(&entry)
        })
        .and_then(|log| {
            ensure!(at_checkpoint.is_some(), "the log ends before the daily snapshot's checkpoint at {offset}");
            Ok(log)
        });
        let log = replay.settled(read)?;
        Ok((replay, log))
    }
}
