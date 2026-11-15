//! The rule's pure limits: how long a read that rules promotions out holds, when the log lags its
//! confirmations, and which heartbeat rules the node refuses to sequence under.

use std::time::Duration;

use crate::liveness::chain::Rule;
use crate::liveness::{DELTA, HEAD_ROUND_TRIP, Released, Signable};

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
pub(super) fn refused(rule: Rule, tick: Duration) -> Option<&'static str> {
    let window = Duration::from_secs(rule.interval.saturating_mul(u64::from(rule.misses)));
    if window <= DELTA + 2 * HEAD_ROUND_TRIP {
        return Some("heartbeatMisses x heartbeatInterval is at or below the stop margin plus two head round trips");
    }
    if tick > rule.interval() / 2 {
        return Some("the ticker interval is above half the heartbeat interval");
    }
    None
}
