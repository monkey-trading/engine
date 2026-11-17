//! Calendar validation uses the same schedule predicate as execution, including dated exceptions.

use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, LocalResult, NaiveDateTime, Offset, TimeZone, Utc};

use crate::session::{Schedule, WEEKEND_SEARCH_MS};
use crate::types::Millis;

/// How far after scheduling the session closes before a corporate action are looked for: the year the
/// weekend gap searches look ahead.
const CLOSE_SEARCH_MS: Millis = WEEKEND_SEARCH_MS;
/// A day of sequenced time, the least lead each trading day past the first adds.
const DAY_MS: Millis = 24 * 60 * 60 * 1000;

/// Checks an actual close and its immediately following venue opening.
/// Error cases:
/// - either boundary is not a transition, or another opening lies between the two
pub(super) fn validate_boundaries(schedule: &Schedule, close: Millis, open: Millis) -> Result<(), String> {
    if !schedule.is_open(close - 1) || schedule.is_open(close) || schedule.is_open(open - 1) || !schedule.is_open(open)
    {
        return Err("corporate boundaries must be an actual venue close and open".into());
    }
    let Schedule::Weekly { tz, windows, closed_dates, closures, special_sessions } = schedule else {
        return Err("corporate actions require a venue calendar with close/open boundaries".into());
    };
    let start = DateTime::<Utc>::from_timestamp_millis(close).ok_or("invalid close timestamp")?;
    let end = DateTime::<Utc>::from_timestamp_millis(open).ok_or("invalid open timestamp")?;
    // Local dates can move backwards at an offset change. Every supported offset is less
    // than one day, so these UTC bounds include every local date crossed by the interval.
    let mut date = start.date_naive().pred_opt().expect("positive engine timestamp");
    let end_date = end.date_naive().succ_opt().expect("engine timestamps end in year 9999");
    let intervening = |at: Millis| at > close && at < open && schedule.is_open(at) && !schedule.is_open(at - 1);
    let local_boundaries = closures.iter().chain(special_sessions).flat_map(|p| [p.from, p.to]).chain(
        closed_dates.iter().flat_map(|d| [Some(*d), d.succ_opt()]).flatten().filter_map(|d| d.and_hms_opt(0, 0, 0)),
    );
    for local in local_boundaries {
        if local_candidates(*tz, local).into_iter().any(&intervening) {
            return Err("previous_close is not the close immediately before ex_open".into());
        }
    }
    while date <= end_date {
        let mut resume = date;
        for boundary in windows.iter().flat_map(|window| [window.open, window.close]) {
            let minute = boundary.minute_of_week();
            if date.weekday().num_days_from_monday() == minute / 1440 {
                let local = date.and_hms_opt((minute % 1440) / 60, minute % 60, 0).expect("valid week time");
                if local_candidates(*tz, local).into_iter().any(&intervening) {
                    return Err("previous_close skips an intervening venue session".into());
                }
                // A dated closure can span years. Its end and every special reopening were
                // checked above, so skip its covered days instead of scanning every civil day.
                for closure in closures.iter().filter(|p| p.from <= local && local < p.to) {
                    resume = resume.max(closure.to.date().min(end_date));
                }
            }
        }
        if resume > date {
            date = resume;
            continue;
        }
        let Some(next) = date.succ_opt() else { break };
        date = next;
    }
    Ok(())
}

/// Whether `schedule` has at least `count` trading days between `after` and `close`: sessions closing
/// after `after` and at or before `close`, within `CLOSE_SEARCH_MS` of `after`, on at least `count`
/// distinct local dates, with `close` at least a day after `after` for each trading day past the
/// first. The dates keep closes within one day from counting twice, and the time keeps sessions the
/// calendar adds at once from shortening the lead below a day each.
pub(super) fn trading_days_at_least(schedule: &Schedule, after: Millis, close: Millis, count: u32) -> bool {
    let Schedule::Weekly { tz, .. } = schedule else { return false };
    if close - after < i64::from(count.saturating_sub(1)) * DAY_MS {
        return false;
    }
    let limit = close.min(after.saturating_add(CLOSE_SEARCH_MS));
    let mut dates = BTreeSet::new();
    let mut at = after;
    while dates.len() < count as usize {
        let Some(change) = schedule.next_change(at, limit) else { return false };
        // Open at `at`, the venue shuts at `change`: a session closes there, on the local date of its
        // last open instant.
        if schedule.is_open(at) {
            let last = DateTime::<Utc>::from_timestamp_millis(change - 1).expect("a close within engine time");
            dates.insert(last.with_timezone(tz).date_naive());
        }
        at = change;
    }
    true
}

/// Boundary occurrences and the offset change that can cross it backwards or skip over it.
fn local_candidates(tz: chrono_tz::Tz, local: NaiveDateTime) -> Vec<Millis> {
    match tz.from_local_datetime(&local) {
        LocalResult::Single(at) => vec![first_engine_millisecond(at)],
        LocalResult::Ambiguous(a, b) => {
            let (mut before, mut after) = (a.timestamp_millis(), b.timestamp_millis());
            let offset = a.offset().fix();
            while after - before > 1 {
                let mid = before + (after - before) / 2;
                let at = DateTime::<Utc>::from_timestamp_millis(mid).expect("between valid boundary occurrences");
                if at.with_timezone(&tz).offset().fix() == offset {
                    before = mid;
                } else {
                    after = mid;
                }
            }
            vec![first_engine_millisecond(a), after, first_engine_millisecond(b)]
        }
        LocalResult::None => chrono_tz::GapInfo::new(&local, &tz)
            .and_then(|gap| gap.end)
            .map(first_engine_millisecond)
            .into_iter()
            .collect(),
    }
}

/// The first sequenced millisecond at or after a possibly sub-millisecond calendar boundary.
fn first_engine_millisecond(at: DateTime<chrono_tz::Tz>) -> Millis {
    at.timestamp_millis() + i64::from(!at.timestamp_subsec_nanos().is_multiple_of(1_000_000))
}
