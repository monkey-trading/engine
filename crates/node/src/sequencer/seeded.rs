//! A sequencer that takes over an engine no log holds: a book assembled in memory
//! (`common::launch_book`), which a benchmark or a load test runs a node on.

use anyhow::Context;
use engine_core::Engine;
use engine_core::command::Command;

use crate::sequencer::Sequencer;

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
}
