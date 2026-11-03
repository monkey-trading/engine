//! Sequencing a venue's quote. The feed bounds a quote's age when it reads it, but the host can hold
//! the sequencer back after that, by stalling its log or its CPU, so the bound is checked again at
//! the stamp.

use std::time::Instant;

use engine_core::command::Command;
use engine_core::event::Event;
use tracing::debug;

use crate::sequencer::Sequencer;

impl Sequencer {
    /// Sequences `cmd`, a venue's quote, unless the monotonic clock has passed `not_after`, when it
    /// is dropped as no quote: returns `None`, as for a stamp trusted time refuses.
    /// Error cases:
    /// - as `sequence`
    pub(super) fn sequence_quote(
        &mut self,
        cmd: Command,
        not_after: Option<Instant>,
    ) -> anyhow::Result<Option<Vec<Event>>> {
        if not_after.is_some_and(|not_after| self.clock.instant() > not_after) {
            debug!("venue quote reached the sequencer past its delay bound: dropped");
            return Ok(None);
        }
        self.sequence(cmd)
    }
}
