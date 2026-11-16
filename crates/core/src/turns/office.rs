//! The operator's share's view of the turns: who holds the office, and until when.

use super::turn_end;
use crate::engine::Engine;
use crate::protocol::ProtocolVersion;
use crate::turns::TurnReason;
use crate::types::{AccountId, Millis};

impl Engine {
    /// From `V51`, while a turn is in effect, its member while it holds the turn it took, with the end
    /// of that turn, or from `V71`, for a promotion, until the next turn applies, which a promotion
    /// finalised after its turn's end needs; `None` before its `TurnTaken`. Turning rotation off leaves
    /// drawn turns standing, and their members earning, as the registry still names them. The outer
    /// `None` means no turn has applied, so the earlier rule names the account in office.
    pub(crate) fn turn_office(&self) -> Option<Option<(AccountId, Millis)>> {
        if self.protocol_version < ProtocolVersion::V51 {
            return None;
        }
        let in_effect = self.daily_turns.in_effect?;
        let taken = self
            .daily_turns
            .taken
            .is_some_and(|taken| (taken.turn, taken.position) == (in_effect.turn, in_effect.position));
        // From `V71` a promotion holds the office until the next turn applies, whichever side of its
        // turn's end it was mirrored on, as the schedule keeps naming it while no later turn is drawn.
        let promoted = self.protocol_version >= ProtocolVersion::V71 && in_effect.reason == TurnReason::Promotion;
        let until = if promoted { self.first_turn_applies_at() } else { turn_end(in_effect.turn) };
        Some(taken.then_some((in_effect.candidate, until)))
    }

    /// From `V53`, when the first accepted turn waiting to apply does, which ends the office of the
    /// account in it before any turn is in effect, and from `V71` that of a promotion; `Millis::MAX` before `V53` or with none waiting.
    pub(crate) fn first_turn_applies_at(&self) -> Millis {
        if self.protocol_version < ProtocolVersion::V53 {
            return Millis::MAX;
        }
        self.daily_turns.assigned.values().map(|assigned| assigned.applies_at).min().unwrap_or(Millis::MAX)
    }
}
