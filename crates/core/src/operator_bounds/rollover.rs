//! The rollover's bounds, from `V76`. The rollover time, its time zone, hour and minute, moves only as a
//! parameter notice of at least seven days names it, or by a vote. A new rollover time leaves the
//! rollover already due as it is, and applies from the one after it. And a rollover charges a day's
//! financing in proportion to the sequenced time since the last one, so financing runs at a day's per
//! day, whatever the schedule.

use crate::Fixed;
use crate::engine::Engine;
use crate::fixed::Rounding;
use crate::params::Rollover;
use crate::protocol::ProtocolVersion;
use crate::types::Millis;

/// A day of sequenced time, which a rollover charges a day's financing for.
pub const ROLLOVER_DAY_MS: Millis = 24 * 60 * 60 * 1000;

impl Engine {
    /// Checks the rollover time a notice names, if any.
    /// Error cases:
    /// - before `V76`
    /// - a time `Rollover::validate` refuses
    pub(crate) fn check_rollover_notice(&self, rollover: Option<&Rollover>) -> Result<(), String> {
        let Some(rollover) = rollover else { return Ok(()) };
        if self.protocol_version < ProtocolVersion::V76 {
            return Err(format!("a rollover notice needs protocol {:?}", ProtocolVersion::V76));
        }
        rollover.validate()
    }

    /// From `V76`, checks parameters change the rollover time only as a notice whose time has come names
    /// it. A `voted` change's notice is its vote's.
    /// Error cases:
    /// - from `V76`, unless `voted`, a changed rollover time no notice names, or whose notice takes
    ///   effect later than the sequenced time
    pub(crate) fn check_rollover_change(
        &self,
        rollover: &Rollover,
        current: &Rollover,
        voted: bool,
    ) -> Result<(), String> {
        if rollover == current || voted || self.protocol_version < ProtocolVersion::V76 {
            return Ok(());
        }
        let named = self.operator_bounds.notice.as_ref().filter(|notice| notice.rollover.as_ref() == Some(rollover));
        match named {
            None => Err("a rollover change needs a notice of at least seven days".into()),
            Some(notice) if notice.effective_at > self.now => {
                Err(format!("the rollover change takes effect from {}", notice.effective_at))
            }
            Some(_) => Ok(()),
        }
    }

    /// The next rollover once `rollover` is in force from the sequenced time: its first time after now,
    /// and from `V76` the rollover already due, if one is, so a new time neither brings it earlier nor
    /// skips it, and applies from the rollover after it.
    pub(crate) fn rescheduled_rollover(&self, rollover: &Rollover) -> Millis {
        match self.next_rollover {
            Some(due) if self.protocol_version >= ProtocolVersion::V76 => due,
            _ => rollover.next_after(self.now),
        }
    }

    /// The sequenced time the rollover at `at` charges for: from `V76` the time since the last rollover,
    /// and a whole day for the first one of a log that sets its parameters under `V76`; a whole day
    /// before it.
    pub(crate) fn rollover_span(&self, at: Millis) -> Millis {
        match self.operator_bounds.last_rollover {
            Some(last) if self.protocol_version >= ProtocolVersion::V76 => (at - last).max(0),
            _ => ROLLOVER_DAY_MS,
        }
    }

    /// When `version` activates `V76` over parameters already set, takes the rollover before the one due,
    /// by the time in force, as the last, which the first span under `V76` runs from.
    pub(crate) fn start_rollover_spans(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V76 || version < ProtocolVersion::V76 {
            return;
        }
        if let (Some(params), Some(due)) = (&self.params, self.next_rollover) {
            self.operator_bounds.last_rollover = Some(params.rollover.previous_before(due));
        }
    }

    /// From `V76`, records the rollover at `at` as the last, which the next one's span runs from.
    pub(crate) fn record_rollover(&mut self, at: Millis) {
        if self.protocol_version >= ProtocolVersion::V76 {
            self.operator_bounds.last_rollover = Some(at);
        }
    }
}

/// `amount`, a day's financing, over `span` of a day, rounded up, in the house's favour: a charge
/// rounds up and a credit toward zero. A whole day is `amount` itself.
pub(crate) fn financing_over(amount: Fixed, span: Millis) -> Fixed {
    if span == ROLLOVER_DAY_MS {
        return amount;
    }
    amount.mul_div(Fixed::from_int(span), Fixed::from_int(ROLLOVER_DAY_MS), Rounding::Up)
}
