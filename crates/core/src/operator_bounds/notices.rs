//! From `V88`, how long a notice can be used. A change a notice names applies from the notice's time to
//! `NOTICE_USE_MS` after it, and needs a new notice after that. A notice left unused past its time tells
//! no one when its change will land, so the operator could hold one armed and apply it at a moment of its
//! choosing: a margin raise when accounts sit just above the new level, or price rules no venue set meets
//! when a market holds positions.

use crate::engine::Engine;
use crate::operator_bounds::NOTICE_MS;
use crate::protocol::ProtocolVersion;
use crate::types::Millis;

/// How long after its time a notice can be used: seven days, the length of the notice itself.
pub const NOTICE_USE_MS: Millis = NOTICE_MS;

/// When a notice taking effect at `effective_at` lapses.
pub fn lapse_of(effective_at: Millis) -> Millis {
    effective_at.saturating_add(NOTICE_USE_MS)
}

impl Engine {
    /// From `V88`, whether a notice taking effect at `effective_at` has lapsed by the sequenced time.
    pub(crate) fn notice_lapsed(&self, effective_at: Millis) -> bool {
        self.protocol_version >= ProtocolVersion::V88 && self.now > lapse_of(effective_at)
    }

    /// Checks a notice taking effect at `effective_at` can be used now for `change`, which names the change
    /// in the refusal: its time has come and, from `V88`, it has not lapsed. Every site that spends a
    /// notice checks it here, so none checks its time without its lapse.
    /// Error cases:
    /// - `effective_at` is later than the sequenced time
    /// - from `V88`, the notice has lapsed (`Engine::notice_lapsed`)
    pub(crate) fn check_notice_in_force(&self, effective_at: Millis, change: &str) -> Result<(), String> {
        if effective_at > self.now {
            return Err(format!("{change} takes effect from {effective_at}"));
        }
        if self.notice_lapsed(effective_at) {
            return Err(format!("{change} needs a new notice: its notice lapsed at {}", lapse_of(effective_at)));
        }
        Ok(())
    }
}
