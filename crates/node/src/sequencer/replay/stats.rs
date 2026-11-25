//! Replay's stats record: the dashboard series, activity and scans recovery rebuilds from the log.

use engine_core::Engine;
use engine_core::command::Sequenced;
use engine_core::event::Event;

use crate::sequencer::replay::Replay;
use crate::stats::Stats;

impl Replay {
    /// Has this replay record every command it applies into `stats`, which starts over: a recovery
    /// attempt that failed part way leaves nothing in it.
    pub(crate) fn record_stats_with(&mut self, stats: Stats) {
        stats.restart();
        self.stats = Some(stats);
    }

    /// Has this replay record every command it applies into `stats` as it stands: for a replay that goes on from a
    /// daily snapshot's checkpoint, into the record the snapshot carries (`Stats::restore`), or one started over
    /// there when it carries none.
    pub(crate) fn record_stats_continuing(&mut self, stats: Stats) {
        self.stats = Some(stats);
    }

    /// Starts the stats worker over a copy of the state replayed so far, if this replay records stats: each
    /// command it applies from here reaches the worker as it is applied (`record`), so a start from this
    /// replay's state takes the worker on and copies nothing.
    pub(crate) fn warm_stats(&self) {
        if let Some(stats) = &self.stats {
            stats.scan_in_background(&self.engine);
        }
    }
}

/// Records `sequenced`, just applied to `engine` with `events`, into `stats`, a replay's record if it keeps
/// one, and hands its worker, if one runs, the command and the scans it made due. The worker takes each at
/// once, not with a batch's sync as the sequencer's does: nothing reads a replica's record before its
/// start, whose recovery syncs the log first.
pub(super) fn record(stats: Option<&Stats>, engine: &Engine, sequenced: &Sequenced, events: &[Event]) {
    if let Some(stats) = stats {
        stats.follow(sequenced);
        stats.observe(engine, sequenced.ts, events);
        stats.deliver(stats.stage());
    }
}
