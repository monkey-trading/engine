//! A sequencer that takes over an engine no log holds: a book assembled in memory
//! (`common::launch_book`), which a benchmark or a load test runs a node on, and the reads that recover
//! such a log as a node recovers a whole one.

use std::path::Path;

use anyhow::Context;
use engine_core::Engine;
use engine_core::command::Command;
use engine_core::types::AccountId;

use crate::log::{CheckpointDomain, LogEntry, ReplayWorkers};
use crate::sequencer::replay::Replay;
use crate::sequencer::{ReplayedLog, Sequencer};

impl Sequencer {
    /// Takes over `engine`, as recovery takes over a replayed one: the next seq, the last stamp and
    /// the last checkpoint follow the engine's. The log holds only what is sequenced from here, which
    /// a replay from the same engine checks (`replay::replay_path_from`).
    pub(crate) fn take_over(&mut self, engine: Engine) {
        self.next_seq = engine.last_seq().map_or(0, |seq| seq + 1);
        self.last_ts = engine.now();
        self.last_checkpoint = engine.last_seq();
        self.engine = engine;
    }

    /// `take_over`, then a tick stamped and a checkpoint written and flushed: the state tree's first
    /// root, over every account the engine holds, is taken here, before anything a caller times. A
    /// trusted time sample must already admit the stamp.
    /// Error cases:
    /// - trusted time refuses the stamp, or the log cannot be written
    pub(crate) fn take_over_checkpointed(&mut self, engine: Engine) -> anyhow::Result<()> {
        self.take_over(engine);
        self.sequence(Command::Tick)?.context("trusted time admits the first stamp")?;
        self.checkpoint()?;
        self.flush()
    }

    /// `take_over_checkpointed` with the dashboard record `stats` tracked from before the first command, so the stats
    /// worker's fold holds every entry of the log, as the daily snapshots need it to carry the replay state.
    /// Error cases:
    /// - trusted time refuses the stamp, or the log cannot be written
    pub(crate) fn take_over_tracked(&mut self, engine: Engine, stats: crate::stats::Stats) -> anyhow::Result<()> {
        self.take_over(engine);
        self.track_stats(stats);
        self.sequence(Command::Tick)?.context("trusted time admits the first stamp")?;
        self.checkpoint()?;
        self.flush()
    }

    /// Reads the log in `dir`, written by a sequencer that took over `base`, as recovery reads a whole
    /// log (`read_log_with_authorities`): replayed from `base` under `signer` in `domain`, each mark
    /// handed to `marks` and each command recorded into `stats`, `observe` seeing each entry first, and
    /// the command seals checked ahead by `workers`.
    /// Error cases:
    /// - the log cannot be read, or fails its manifest check or replay
    pub(crate) fn read_log_from(
        base: Engine,
        dir: &Path,
        (domain, signer): (CheckpointDomain, AccountId),
        marks: Option<crate::history::marks::MarkSink>,
        stats: Option<crate::stats::Stats>,
        observe: impl FnMut(&LogEntry),
        workers: ReplayWorkers,
    ) -> anyhow::Result<ReplayedLog> {
        Self::replay_log(Replay::seeded(base, Some(domain), signer), dir, observe, marks, stats, workers, None)
    }
}
