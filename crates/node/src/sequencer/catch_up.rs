//! Finishes a catch-up before a command acts. From `V33` the engine applies only a bounded batch of
//! rollovers and corporate boundaries per command and defers the command (`Event::CatchUpDeferred`).
//! The sequencer sequences `Tick`s while the engine reports a catch-up pending, so a command is not
//! deferred. An applied benchmark snapshot goes first instead, because the rollovers need its rates;
//! the engine reports its outcome with the deferral and the `Tick`s follow. A rejected snapshot and a
//! protocol activation run no catch-up, so they drain nothing.
//!
//! A client command whose stamp passes a due rollover, MONKEY period close or epoch close takes a
//! `Tick` at its own stamp first, so the mass event's events sit in the `Tick`'s entry and the
//! client's reply holds only its own.

use anyhow::{Result, bail};
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::types::Millis;
use tracing::info;

use crate::sequencer::{Sequencer, Stamped};

impl Sequencer {
    /// Stamps and sequences `cmd` at `ts`, after the catch-up the engine owes before `ts`.
    /// `None`, with nothing logged, when a detected rollback froze sequencing, or the liveness rule's
    /// fence closed, while the request was admitted: the remaining ticks and the command itself are
    /// refused, like an unadmitted stamp.
    /// Error cases:
    /// - as `stamp_once`
    /// - the engine defers a command although the catch-up was drained, which means the engine's
    ///   deferral and `Engine::catch_up_pending` disagree
    pub(super) fn stamp(&mut self, ts: Millis, cmd: Command) -> Result<Option<Stamped>> {
        if self.stopped() {
            return Ok(None);
        }
        let snapshot = cmd.is_benchmark_batch();
        let activation = cmd.is_dated_rate_activation() || cmd.is_activation();
        if !snapshot && !activation {
            self.drain_catch_up(ts)?;
        }
        if self.stopped() {
            // The latch tripped, or the fence closed, while the catch-up drained (a blocked log
            // write, say): refuse the command itself, like an unadmitted stamp.
            return Ok(None);
        }
        if !snapshot && !activation && carries_reply(&cmd) && self.engine.mass_event_due(ts) {
            self.stamp_once(ts, Command::Tick)?;
            if self.stopped() {
                return Ok(None);
            }
        }
        let events = self.stamp_once(ts, cmd)?;
        if self.stopped() {
            // The latch tripped, or the fence closed, between the request's admission checks and the
            // stamp itself: `stamp_once` refused it, and a refused stamp is never acknowledged as
            // applied.
            return Ok(None);
        }
        if is_deferred(&events) {
            if !snapshot {
                bail!("the engine deferred a command after the catch-up was drained");
            }
            self.drain_catch_up(ts)?;
        }
        Ok(Some(events))
    }

    /// Sequences `Tick`s until no command at `ts` would be deferred. The `Tick`s are stamped one
    /// millisecond before `ts` where the log allows, so the command's own stamp keeps its ordering
    /// (an incoming quote installs before the rollover at its stamp).
    /// Error cases:
    /// - a `Tick` leaves the catch-up pending without being deferred, which no later `Tick` would fix
    fn drain_catch_up(&mut self, ts: Millis) -> Result<()> {
        let tick_at = if self.last_ts < ts { ts - 1 } else { ts };
        let mut ticks = 0_u64;
        while self.engine.catch_up_pending(ts) && !self.stopped() {
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

/// Whether `cmd` is a client's signed request, whose reply is its events: a mass event stays out of
/// them. Admin, custody and feed commands answer no client, and tests and tools read the rollover in
/// the events of the command that carried it.
fn carries_reply(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::AttestEligibility { .. }
            | Command::AccessOrder { .. }
            | Command::AccessWithdraw { .. }
            | Command::AccessCloseAll { .. }
            | Command::AccessSetPositionLevels { .. }
            | Command::PlaceOrder(_)
            | Command::CancelOrder { .. }
            | Command::CancelAll { .. }
            | Command::Withdraw { .. }
            | Command::RegisterSessionKey { .. }
            | Command::RevokeSessionKey { .. }
    )
}

/// Whether `events` carry the engine's deferral of a catch-up.
fn is_deferred(events: &[Event]) -> bool {
    events.iter().any(|event| matches!(event, Event::CatchUpDeferred { .. }))
}
