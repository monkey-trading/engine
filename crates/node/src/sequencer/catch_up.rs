//! Finishes a catch-up before a command acts. From `V33` the engine applies only a bounded batch of
//! rollovers and corporate boundaries per command and defers the command (`Event::CatchUpDeferred`).
//! The sequencer sequences `Tick`s while the engine reports a catch-up pending, so a command is not
//! deferred. An applied benchmark snapshot goes first instead, because the rollovers need its rates;
//! the engine reports its outcome with the deferral and the `Tick`s follow. A rejected snapshot and a
//! protocol activation run no catch-up, so they drain nothing.

use anyhow::{Result, bail};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::types::Millis;
use tracing::info;

use crate::sequencer::Sequencer;

impl Sequencer {
    /// Stamps and sequences `cmd` at `ts`, after the catch-up the engine owes before `ts`.
    /// Error cases:
    /// - as `stamp_once`
    /// - the engine defers a command although the catch-up was drained, which means the engine's
    ///   deferral and `Engine::catch_up_pending` disagree
    pub(super) fn stamp(&mut self, ts: Millis, cmd: Command) -> Result<Vec<Event>> {
        let snapshot = cmd.is_benchmark_batch();
        let activation =
            cmd.is_dated_rate_activation() || matches!(cmd, Command::Admin(AdminCommand::ActivateProtocol { .. }));
        if !snapshot && !activation {
            self.drain_catch_up(ts)?;
        }
        let events = self.stamp_once(ts, cmd)?;
        if is_deferred(&events) {
            if !snapshot {
                bail!("the engine deferred a command after the catch-up was drained");
            }
            self.drain_catch_up(ts)?;
        }
        Ok(events)
    }

    /// Sequences `Tick`s until no command at `ts` would be deferred. The `Tick`s are stamped one
    /// millisecond before `ts` where the log allows, so the command's own stamp keeps its ordering
    /// (an incoming quote installs before the rollover at its stamp).
    /// Error cases:
    /// - a `Tick` leaves the catch-up pending without being deferred, which no later `Tick` would fix
    fn drain_catch_up(&mut self, ts: Millis) -> Result<()> {
        let tick_at = if self.last_ts < ts { ts - 1 } else { ts };
        let mut ticks = 0_u64;
        while self.engine.catch_up_pending(ts) {
            let events = self.stamp_once(tick_at, Command::Tick)?;
            if !is_deferred(&events) && self.engine.catch_up_pending(ts) {
                bail!("a tick finished its catch-up but the engine still reports one pending");
            }
            ticks += 1;
        }
        if ticks > 0 {
            info!(ticks, last_seq = ?self.engine.last_seq(), "caught up on rollovers and corporate boundaries");
        }
        Ok(())
    }
}

/// Whether `events` carry the engine's deferral of a catch-up.
fn is_deferred(events: &[Event]) -> bool {
    events.iter().any(|event| matches!(event, Event::CatchUpDeferred { .. }))
}
