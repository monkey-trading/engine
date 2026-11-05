//! A daily turn's `TurnTaken`: the first command this node sequences in each turn the log assigns its
//! operator key, whether it took the keys at the boundary or holds the next turn too.

use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::types::Millis;
use tracing::{info, warn};

use crate::sequencer::Sequencer;

impl Sequencer {
    /// Sequences `TurnTaken` at `ts` when the turn due by `ts`, or the one in effect, names this
    /// node's operator key and has not been taken under that assignment: the engine hands the turn's
    /// key over before it authorises the command, so the turn is the one in effect when it applies.
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
        let Some(events) = self.stamp(ts, Command::TurnTaken { turn: turn.turn, release })? else { return Ok(()) };
        match events.last() {
            Some(Event::TurnTaken { .. }) => info!(turn = turn.turn, position = turn.position, "took the turn"),
            other => warn!(turn = turn.turn, "the engine did not record this node's turn: {other:?}"),
        }
        Ok(())
    }
}
