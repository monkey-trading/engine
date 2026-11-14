//! A daily turn's `TurnTaken`: the first command this node sequences in each turn the log assigns its
//! operator key, whether it took the keys at the boundary or holds the next turn too. The engine runs
//! no catch-up in it, so it is stamped alone, ahead of any catch-up `Tick`, and behind a benchmark
//! hold too. With sealed keys it waits until they are sealed in the generation the finalised schedule
//! holds for the turn, sealing them there first when the schedule names this release in a new one.

use std::collections::BTreeMap;
use std::sync::Arc;

use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::turns::DailyTurns;
use engine_core::types::{AccountId, Millis};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::sequencer::Sequencer;
use crate::tee::sealing::KeyStore;

/// How long, in sequenced time, a failed seal in a turn's generation waits before the next attempt,
/// so a failing disk is not written on every command.
const RESEAL_RETRY_MS: Millis = 10_000;

/// A node's sealed keys, and the generation the finalised schedule holds for each turn the log
/// assigns its operator key and names its release for (`node::turn_keys`), which its `TurnTaken`
/// waits on.
pub(crate) struct TurnSealing {
    store: Arc<KeyStore>,
    generations: watch::Receiver<BTreeMap<u64, u64>>,
    /// The turn last logged waiting for its generation, so each logs once.
    waiting: Option<u64>,
    /// The turn whose seal in its generation last failed, and when.
    failed: Option<(u64, Millis)>,
}

impl TurnSealing {
    /// Seals `store`'s keys in the generations `generations` publishes, by turn.
    pub(crate) fn new(store: Arc<KeyStore>, generations: watch::Receiver<BTreeMap<u64, u64>>) -> Self {
        Self { store, generations, waiting: None, failed: None }
    }

    /// Whether the keys are sealed in the generation the finalised schedule holds for `turn`, once
    /// it is known, sealing them there first when it is another: the schedule gives a fresh
    /// generation to a holder returning after a turn no one took, and a restart opens only that one.
    /// Called on the sequencer's thread before the turn's `TurnTaken`, at `ts`, so it never follows
    /// the stop of a handover that sent the keys away. A failed seal leaves the turn untaken, to retry
    /// `RESEAL_RETRY_MS` on; `release_superseded` pages once the turn's handover grace has passed.
    fn sealed_for(&mut self, turn: u64, ts: Millis) -> bool {
        let Some(generation) = self.generations.borrow().get(&turn).copied() else {
            if self.waiting.replace(turn) != Some(turn) {
                info!(turn, "waiting for the finalised schedule's generation of this node's turn before taking it");
            }
            return false;
        };
        let held = self.store.generation();
        if generation == held {
            return true;
        }
        if self.failed.is_some_and(|(failed, at)| failed == turn && ts < at.saturating_add(RESEAL_RETRY_MS)) {
            return false;
        }
        match self.store.reseal(generation) {
            Ok(()) => {
                self.failed = None;
                info!(
                    turn,
                    from = held,
                    generation,
                    "sealed the keys in the generation the schedule holds for the turn"
                );
                true
            }
            Err(e) => {
                if self.failed.is_none_or(|(failed, _)| failed != turn) {
                    warn!(turn, generation, "sealing the keys in the turn's generation failed: {e:#}");
                }
                self.failed = Some((turn, ts));
                false
            }
        }
    }
}

impl Sequencer {
    /// The turns the log assigns this node's operator key that it has yet to see taken (`untaken`).
    pub(crate) fn untaken_turns(&self) -> Vec<u64> {
        untaken(self.engine.daily_turns(), self.operator.address)
    }

    /// Sequences `TurnTaken` at `ts` when the turn due by `ts`, or the one in effect, names this
    /// node's operator key and has not been taken under that assignment, and, with sealed keys, they
    /// are sealed in the turn's generation (`TurnSealing::sealed_for`): the engine hands the turn's key
    /// over before it authorises the command, so the turn is the one in effect when it applies.
    /// Error cases:
    /// - as `stamp`
    pub(super) fn take_turn(&mut self, ts: Millis) -> anyhow::Result<()> {
        let Some(release) = self.turn_release else { return Ok(()) };
        let turns = self.engine.daily_turns();
        let Some(turn) = self.engine.due_turn(ts).or(turns.in_effect) else { return Ok(()) };
        let taken = turns.taken.is_some_and(|taken| (taken.turn, taken.position) == (turn.turn, turn.position));
        if turn.admin_key != self.operator.address || taken {
            return Ok(());
        }
        if self.frozen() {
            return Ok(());
        }
        if self.turn_sealing.as_mut().is_some_and(|sealing| !sealing.sealed_for(turn.turn, ts)) {
            return Ok(());
        }
        let events = self.stamp_once(ts, Command::TurnTaken { turn: turn.turn, release })?;
        match events.last() {
            Some(Event::TurnTaken { .. }) => info!(turn = turn.turn, position = turn.position, ts, "took the turn"),
            other => warn!(turn = turn.turn, "the engine did not record this node's turn: {other:?}"),
        }
        Ok(())
    }
}

/// The turns `turns` assigns `operator`, waiting to apply or in effect, that are not taken.
pub(crate) fn untaken(turns: &DailyTurns, operator: AccountId) -> Vec<u64> {
    let taken = turns.taken.map(|taken| (taken.turn, taken.position));
    turns
        .assigned
        .values()
        .chain(turns.in_effect.as_ref())
        .filter(|assigned| assigned.admin_key == operator && taken != Some((assigned.turn, assigned.position)))
        .map(|assigned| assigned.turn)
        .collect()
}

#[cfg(test)]
mod tests;
