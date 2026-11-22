//! The rule's pure limits: how long a read that rules promotions out holds, when the log lags its
//! confirmations, and which heartbeat rules the node refuses to sequence under.

use std::time::Duration;

use crate::liveness::chain::Rule;
use crate::liveness::{DELTA, HEAD_ROUND_TRIP, Released, Signable, read_every};
use crate::seed_relay::schedule::next_boundary;

/// The earliest a promotion can pass after a read showing no turn naming the operator, or no schedule,
/// where `turnable` is the first boundary a drawn turn could start at (`chain::first_turnable`): no
/// slot can be witnessed before a turn names it, so its count then needs `heartbeatMisses` slots, more
/// than `2 x DELTA` under any rule the operator takes (`refused`); the fence holds through that boundary
/// while reads go on.
pub(super) fn unturned(turnable: u64) -> u64 {
    turnable.saturating_add(2 * DELTA.as_secs())
}

/// Whether the log at `tip` has run unconfirmed for more than one heartbeat interval of sequenced
/// time, from `since`, the stamp of the first command past the newest `confirmed` seq, if any; or no
/// seq is confirmed.
pub(super) fn lagging(tip: Released, confirmed: Option<Signable>, since: Option<u64>, rule: Rule) -> bool {
    let interval_ms = rule.interval.saturating_mul(1000);
    match (confirmed, since) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(_), Some(since)) => tip.stamp.saturating_sub(since) > interval_ms,
    }
}

/// Why the node refuses to sequence under `rule` with a ticker every `tick`: a detection window at or
/// below `DELTA` plus twice the head round trip, or a ticker slower than half an interval, which would
/// leave a live head no newer seq to sign.
pub(crate) fn refused(rule: Rule, tick: Duration) -> Option<&'static str> {
    let window = Duration::from_secs(rule.interval.saturating_mul(u64::from(rule.misses)));
    if window <= DELTA + 2 * HEAD_ROUND_TRIP {
        return Some("heartbeatMisses x heartbeatInterval is at or below the stop margin plus two head round trips");
    }
    if tick > rule.interval() / 2 {
        return Some("the ticker interval is above half the heartbeat interval");
    }
    None
}

/// Whether the chain is due a read, `since` after the last one and with `reached` trusted time's lower
/// bound in milliseconds: before the first, `read_every` after the last (a quarter of the last read's
/// heartbeat interval, or the `configured` read interval while it names none), or once trusted time
/// reaches the turn boundary after the last read's block, from which another set may apply. `last` is
/// that read's block time and heartbeat interval, in seconds.
pub(super) fn read_due(
    configured: Duration,
    since: Option<Duration>,
    last: Option<(u64, u64)>,
    reached: Option<u64>,
) -> bool {
    let (Some(since), Some((time, interval))) = (since, last) else { return true };
    let boundary = next_boundary(time).saturating_mul(1000);
    since >= read_every(configured, interval) || reached.is_some_and(|now| now >= boundary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rule_reads_at_its_interval_four_times_a_slot_and_at_a_turn_boundary() {
        let every = Duration::from_secs(15);
        let (time, ms) = (1_800_000_000, 1_800_000_000_000);
        let boundary = next_boundary(time) * 1000;
        let secs = |s| Some(Duration::from_secs(s));
        assert!(read_due(every, None, None, None), "the first read is due at once");
        assert!(!read_due(every, secs(14), Some((time, 60)), Some(ms)));
        assert!(read_due(every, secs(15), Some((time, 60)), Some(ms)), "60 s slots: four reads a slot");
        assert!(!read_due(every, secs(4), Some((time, 20)), Some(ms)));
        assert!(read_due(every, secs(5), Some((time, 20)), Some(ms)), "20 s slots: four reads a slot");
        assert!(!read_due(every, secs(34), Some((time, 140)), Some(ms)), "140 s slots: not at the configured 15 s");
        assert!(read_due(every, secs(35), Some((time, 140)), Some(ms)), "140 s slots: four reads a slot");
        assert!(!read_due(every, secs(14), Some((time, 0)), None));
        assert!(read_due(every, secs(15), Some((time, 0)), None), "no schedule: the configured interval");
        assert!(!read_due(every, secs(1), Some((time, 60)), Some(boundary - 1)));
        assert!(read_due(every, secs(1), Some((time, 60)), Some(boundary)), "at the turn boundary, at once");
    }
}
