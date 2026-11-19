//! Whether the engine takes a symbol's quotes now: its market's hours as the engine last confirmed
//! them.

use engine_core::types::{MarketId, Millis};

use crate::feeds::venue::VenueFeed;

impl VenueFeed {
    /// Whether the engine's last confirmed hours have `market` closed at `ts`. A market whose
    /// hours are not known yet counts as open, and the engine refuses a quote outside them.
    pub(super) fn closed(&self, market: MarketId, ts: Millis) -> bool {
        self.schedules.get(&market).is_some_and(|schedule| !schedule.is_open(ts))
    }

    /// Whether the engine's last confirmed hours keep `market` open from a quote's stamp `ts` or
    /// host time `now`, whichever is earlier, to the later of `ts` and an interval after `now`, by
    /// when the sequencer stamps a quote sent now: the span the engine checks, which a stamp ahead
    /// of the host clock stretches. A span longer than `timing.stale_after` holds only a quote too
    /// old to count, so only its two ends are checked.
    pub(super) fn in_session(&self, market: MarketId, ts: Millis, now: Millis) -> bool {
        let Some(schedule) = self.schedules.get(&market) else { return true };
        let interval = Millis::try_from(self.timing.interval.as_millis()).unwrap_or(Millis::MAX);
        let stale_after = Millis::try_from(self.timing.stale_after.as_millis()).unwrap_or(Millis::MAX);
        let (first, last) = (ts.min(now), ts.max(now.saturating_add(interval)));
        if last.saturating_sub(first) > stale_after.saturating_add(interval) {
            return schedule.is_open(first) && schedule.is_open(last);
        }
        schedule.open_throughout(first, last)
    }

    /// Whether any symbol should be quoting at host time `now`: one the engine takes, whose market
    /// is open.
    pub(super) fn expects_quotes(&self, now: Millis) -> bool {
        self.instruments
            .iter()
            .any(|(symbol, instrument)| !self.idle.contains(symbol) && !self.closed(instrument.market, now))
    }
}
