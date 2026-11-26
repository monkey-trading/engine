//! The sequencer's end of the derived marks cache: it reports each market's mark changes, once
//! the command that produced them is durable in the log, to the cache's task at the command's
//! sequenced time, so a cache seeded at recovery continues exactly.

use std::collections::BTreeMap;

use engine_core::command::Sequenced;
use engine_core::types::{MarketId, Millis};
use engine_core::{Engine, Fixed};
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::{debug, info, warn};

use crate::history::marks::store::Frozen;
use crate::history::marks::{Freeze, Report};
use crate::log::Offset;
use crate::sequencer::Sequencer;

/// Live mark reporting for the derived marks cache: the channel, the last mark per market (only
/// changes are reported, and recovery hands its map over so reporting continues exactly where the
/// replay left off), reports pending their command's durability, and one overflow alert per
/// episode.
#[derive(Default)]
pub(super) struct Marks {
    tx: Option<mpsc::Sender<Report>>,
    last: BTreeMap<MarketId, Fixed>,
    pending: Vec<Report>,
    /// Freezes the queue had no room for, sent first at the next release; a sample behind one is dropped, so
    /// no mark of a later command reaches the cache before its frozen copy is written.
    held: Vec<Report>,
    overflow_warned: bool,
    /// The dashboard record every applied command is added to, once the node attaches it.
    stats: Option<crate::stats::Stats>,
}

impl Marks {
    /// Continues change-only reporting from `last`, the map recovery's replay sampled.
    pub fn take_recovered(&mut self, last: BTreeMap<MarketId, Fixed>) {
        self.last = last;
    }

    /// Buffers each listed market whose mark moved on the command just applied, in market order,
    /// at its sequenced time, for `release` once the command is durable in the log.
    fn report(&mut self, engine: &Engine, ts: Millis) {
        for (id, market) in engine.markets() {
            if let Some(mark) = market.mark
                && self.last.insert(*id, mark) != Some(mark)
            {
                self.pending.push(Report::Sample((*id, ts, mark)));
            }
        }
    }

    /// Buffers a daily snapshot's freeze for the checkpoint at `offset`, behind the reports of every command it
    /// covers, for `release` with them, and gives the end the cache's anchors come back on once the marks task has
    /// written the frozen copy, keeping those of the newest snapshots listed in `snapshots`; `None` when no marks task
    /// runs, as on a member.
    pub(super) fn freeze(
        &mut self,
        offset: Offset,
        snapshots: Option<std::path::PathBuf>,
    ) -> Option<std::sync::mpsc::Receiver<Frozen>> {
        self.tx.as_ref()?;
        let (reply, frozen) = std::sync::mpsc::sync_channel(1);
        self.pending.push(Report::Freeze(Freeze { offset, reply, snapshots }));
        Some(frozen)
    }

    /// The dashboard record, whose worker also encodes recovery caches, once the node attaches it.
    pub(super) fn stats(&self) -> Option<&crate::stats::Stats> {
        self.stats.as_ref()
    }

    /// The stats worker's jobs from the batch so far, for `deliver_stats` once it is durable.
    pub(super) fn stage_stats(&self) -> Vec<crate::stats::background::Job> {
        self.stats.as_ref().map(crate::stats::Stats::stage).unwrap_or_default()
    }

    /// Hands the stats worker the jobs of a batch that is durable.
    pub(super) fn deliver_stats(&self, jobs: Vec<crate::stats::background::Job>) {
        if let Some(stats) = &self.stats {
            stats.deliver(jobs);
        }
    }

    /// Takes the buffered reports, for `release` once their commands are durable.
    pub(super) fn take_pending(&mut self) -> Vec<Report> {
        std::mem::take(&mut self.pending)
    }

    /// Sends `reports`, whose commands are durable, after any freeze held back. A full queue drops a
    /// sample: the cache is derived, and the next change or a whole replay reseeds it; one alert per
    /// episode says so, and the first batch the queue takes whole ends the episode. A freeze it has no
    /// room for is held back for the next release, and the samples behind it are dropped meanwhile, so
    /// the episode lasts while one is held.
    pub(super) fn release(&mut self, reports: Vec<Report>) {
        if reports.is_empty() && self.held.is_empty() {
            return;
        }
        let Some(tx) = self.tx.clone() else { return };
        let mut dropped = false;
        let mut closed = false;
        let mut queued = std::mem::take(&mut self.held);
        queued.extend(reports);
        for report in queued {
            if !self.held.is_empty() {
                match report {
                    Report::Freeze(_) => self.held.push(report),
                    Report::Sample(_) => dropped = true,
                }
                continue;
            }
            match tx.try_send(report) {
                Ok(()) => {}
                Err(TrySendError::Full(Report::Freeze(freeze))) => self.held.push(Report::Freeze(freeze)),
                Err(TrySendError::Full(_)) => dropped = true,
                Err(TrySendError::Closed(_)) => closed = true,
            }
        }
        // A freeze still held found the queue full, and the samples behind it go, though this call dropped none.
        dropped |= !self.held.is_empty();
        // The cache task ended (a shutdown, which it flushes through, or a failure its supervisor
        // stops the node for): nothing is lost to a full queue, so no alert, and no more sends.
        if closed {
            debug!("the marks task has ended; marks reports stop");
            self.tx = None;
            return;
        }
        if dropped && !self.overflow_warned {
            warn!(
                alert = "marks_samples_dropped",
                "the marks queue is full; dropped samples reseed at the next whole replay"
            );
            self.overflow_warned = true;
        } else if !dropped && self.overflow_warned {
            info!(alert_resolved = "marks_samples_dropped", "the marks queue takes samples again");
            self.overflow_warned = false;
        }
    }
}

impl Sequencer {
    /// Sends each market's mark changes to `tx` for the derived marks cache. Only changes go, at
    /// the command's sequenced time, once the command is durable, so a cache seeded at recovery
    /// continues exactly.
    pub(crate) fn track_marks(&mut self, tx: mpsc::Sender<Report>) {
        self.marks.tx = Some(tx);
    }

    /// Buffers the mark changes of the command just applied for release with this batch's flush.
    pub(super) fn report_marks(&mut self, sequenced: &Sequenced) {
        self.marks.report(&self.engine, sequenced.ts);
    }

    /// Adds the command just logged, `stamped`, to the dashboard record and hands it to the stats worker
    /// with its cumulative command prefix, for the replay state the worker folds.
    pub(super) fn report_stats(&self, stamped: &super::Stamped) {
        if let (Some(stats), Some(entry)) = (&self.marks.stats, stamped.entry()) {
            stats.follow_logged(entry, engine_core::eip712::B256::from(self.command_prefix));
            stats.observe(&self.engine, entry.sequenced.ts, stamped.as_slice());
        }
    }
}

impl Sequencer {
    /// Adds every command this sequencer applies to the dashboard record `stats`, which recovery
    /// may have seeded, and gives it the recovered state's scan if its replay took none.
    pub fn track_stats(&mut self, stats: crate::stats::Stats) {
        stats.finish_recovery(&self.engine);
        stats.scan_in_background(&self.engine);
        // The worker folds the replay state from where recovery left it, for the daily snapshots.
        if let Some(fold) = self.captures.fold.take() {
            stats.seed_fold(fold);
        }
        self.marks.stats = Some(stats);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks_with(capacity: usize) -> (Marks, mpsc::Receiver<Report>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Marks { tx: Some(tx), ..Marks::default() }, rx)
    }

    fn samples(samples: &[(MarketId, Millis, i64)]) -> Vec<Report> {
        samples.iter().map(|&(market, ts, mark)| Report::Sample((market, ts, Fixed::from_int(mark)))).collect()
    }

    fn sample(report: Report) -> crate::history::marks::MarkSample {
        match report {
            Report::Sample(sample) => sample,
            Report::Freeze(_) => panic!("a freeze where a sample was sent"),
        }
    }

    #[test]
    fn a_full_queue_drops_samples_and_a_batch_it_takes_whole_ends_the_episode() {
        let (mut marks, mut rx) = marks_with(1);
        marks.release(samples(&[(1, 10, 1), (2, 10, 2)]));
        assert!(marks.overflow_warned, "the second sample found the queue full");
        assert_eq!(sample(rx.try_recv().unwrap()).0, 1);
        assert!(rx.try_recv().is_err(), "the dropped sample is not queued");
        // A later batch the queue takes whole clears the episode, so the next overflow alerts again.
        marks.release(samples(&[(1, 20, 3)]));
        assert!(!marks.overflow_warned);
        assert_eq!(sample(rx.try_recv().unwrap()).2, Fixed::from_int(3));
    }

    #[test]
    fn a_closed_queue_is_not_a_drop_and_stops_reporting() {
        let (mut marks, rx) = marks_with(1);
        drop(rx);
        marks.release(samples(&[(1, 10, 1)]));
        assert!(!marks.overflow_warned, "a receiver gone at shutdown is not an overflow");
        assert!(marks.tx.is_none());
    }

    #[test]
    fn an_episode_stays_open_while_the_queue_keeps_dropping() {
        let (mut marks, _rx) = marks_with(1);
        marks.release(samples(&[(1, 10, 1), (2, 10, 2)]));
        marks.release(samples(&[(1, 20, 3)]));
        assert!(marks.overflow_warned, "the queue never drained");
    }

    /// A daily snapshot's freeze goes to the marks task behind the marks of the commands before it, with them, and
    /// none is asked of a sequencer that reports to no marks task.
    #[test]
    fn a_freeze_follows_the_marks_buffered_before_it() {
        let (mut marks, mut rx) = marks_with(8);
        marks.pending = samples(&[(1, 10, 1)]);
        let offset = Offset { seq: 5, index: 2 };
        assert!(marks.freeze(offset, None).is_some());
        let pending = marks.take_pending();
        marks.release(pending);
        assert_eq!(sample(rx.try_recv().unwrap()).0, 1, "the mark before the freeze goes first");
        assert!(matches!(rx.try_recv().unwrap(), Report::Freeze(freeze) if freeze.offset == offset));
        assert!(Marks::default().freeze(offset, None).is_none(), "no marks task, no freeze");
    }

    /// A freeze still held back on a full queue keeps the overflow episode open through a batch that moves no mark, so
    /// the next batch's dropped samples do not page it again.
    #[test]
    fn an_episode_stays_open_while_a_freeze_is_held() {
        let (mut marks, mut rx) = marks_with(1);
        let offset = Offset { seq: 5, index: 2 };
        marks.pending = samples(&[(1, 10, 1)]);
        assert!(marks.freeze(offset, None).is_some());
        marks.pending.extend(samples(&[(2, 11, 2)]));
        let pending = marks.take_pending();
        marks.release(pending);
        assert!(!marks.held.is_empty(), "the freeze found the queue full");
        assert!(marks.overflow_warned, "the mark behind the freeze was dropped");
        marks.release(Vec::new());
        assert!(marks.overflow_warned, "a batch that moves no mark leaves the freeze held and the episode open");
        assert_eq!(sample(rx.try_recv().unwrap()).0, 1);
        marks.release(Vec::new());
        assert!(matches!(rx.try_recv().unwrap(), Report::Freeze(freeze) if freeze.offset == offset));
        assert!(!marks.overflow_warned, "the freeze went, so the queue takes everything it was given");
    }

    /// A freeze the full queue has no room for is held back, not dropped, with the samples behind it dropped so none
    /// reaches the cache before it, and goes first once the queue has room.
    #[test]
    fn a_freeze_waits_for_room_and_no_later_mark_overtakes_it() {
        let (mut marks, mut rx) = marks_with(1);
        let offset = Offset { seq: 5, index: 2 };
        marks.pending = samples(&[(1, 10, 1)]);
        assert!(marks.freeze(offset, None).is_some());
        marks.pending.extend(samples(&[(2, 11, 2)]));
        let pending = marks.take_pending();
        marks.release(pending);
        assert_eq!(sample(rx.try_recv().unwrap()).0, 1, "the mark before the freeze fills the queue");
        assert!(rx.try_recv().is_err(), "neither the freeze nor the mark behind it went");
        assert!(marks.overflow_warned, "the mark behind the freeze was dropped");
        marks.release(samples(&[(1, 12, 3)]));
        assert!(matches!(rx.try_recv().unwrap(), Report::Freeze(freeze) if freeze.offset == offset));
        assert!(rx.try_recv().is_err(), "the next mark waited behind the freeze, and was dropped");
        marks.release(samples(&[(1, 13, 4)]));
        assert_eq!(sample(rx.try_recv().unwrap()).2, Fixed::from_int(4), "marks go again once the freeze has");
    }
}
