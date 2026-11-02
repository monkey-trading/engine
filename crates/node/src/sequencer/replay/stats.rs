//! Replay's stats record: the dashboard series, activity and scans recovery rebuilds from the log.

use crate::sequencer::replay::Replay;
use crate::stats::Stats;

impl Replay {
    /// Has this replay record every command it applies into `stats`, which starts over: a recovery
    /// attempt that failed part way leaves nothing in it.
    pub(crate) fn record_stats_with(&mut self, stats: Stats) {
        stats.restart();
        self.stats = Some(stats);
    }
}
