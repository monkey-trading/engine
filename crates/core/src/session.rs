//! Trading hours. Crypto trades 24/7; RWA markets follow their venue's weekly windows in local
//! time (so DST is handled by the tz database), minus holiday closures and dated closures such as
//! early closes, plus dated special sessions. While a market is closed nothing fills. Resting
//! orders wait, and the first fresh price after the open fills them, gap included.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::types::Millis;

const MINUTES_PER_DAY: u32 = 24 * 60;
const MS_PER_MINUTE: Millis = 60_000;
const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Schedule {
    AlwaysOpen,
    Weekly {
        tz: Tz,
        windows: Vec<Window>,
        /// Whole local days the venue is shut (exchange holidays).
        #[serde(default)]
        closed_dates: Vec<NaiveDate>,
        /// Dated closures inside the weekly windows: early closes, holiday halts, and closures
        /// that span midnight. Skipped from the encoding when empty, so the state root of a
        /// schedule without closures does not depend on this field.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        closures: Vec<Period>,
        /// Dated sessions outside the weekly windows. A special session is open even on a closed
        /// date or inside a closure. Skipped from the encoding when empty, like `closures`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        special_sessions: Vec<Period>,
    },
}

/// `[from, to)` in venue-local date and time, written `"2026-11-27T13:00:00"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Period {
    pub from: NaiveDateTime,
    pub to: NaiveDateTime,
}

/// `[open, close)` in venue-local week time. A window whose close is before its open wraps over
/// the week boundary (e.g. FX: Sun 17:00 to Fri 17:00).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub open: WeekTime,
    pub close: WeekTime,
}

/// Minutes since Monday 00:00 local time, written as `"Mon 09:30"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WeekTime(u32);

impl fmt::Display for WeekTime {
    /// `"Mon 09:30"` form.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let day = DAYS[(self.0 / MINUTES_PER_DAY) as usize];
        let mins = self.0 % MINUTES_PER_DAY;
        write!(f, "{day} {:02}:{:02}", mins / 60, mins % 60)
    }
}

impl WeekTime {
    /// The minute in the venue's civil week, for calendar-boundary validation.
    pub(crate) fn minute_of_week(self) -> u32 {
        self.0
    }
}

impl FromStr for WeekTime {
    type Err = String;

    /// Parses `"<Day> HH:MM"` with a three-letter English day.
    /// Error cases:
    /// - no space between day and time, or no `:` in the time
    /// - an unknown day name
    /// - an hour above 23 or a minute above 59
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || format!("invalid week time {s:?}, expected e.g. \"Mon 09:30\"");
        let (day, hm) = s.split_once(' ').ok_or_else(err)?;
        let day = DAYS.iter().position(|d| *d == day).ok_or_else(err)? as u32;
        let (h, m) = hm.split_once(':').ok_or_else(err)?;
        let h: u32 = h.parse().map_err(|_| err())?;
        let m: u32 = m.parse().map_err(|_| err())?;
        if h > 23 || m > 59 {
            return Err(err());
        }
        Ok(WeekTime(day * MINUTES_PER_DAY + h * 60 + m))
    }
}

impl Serialize for WeekTime {
    /// Serializes in `"Mon 09:30"` form.
    /// Error cases:
    /// - the serializer rejects a string
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for WeekTime {
    /// Deserializes from `"Mon 09:30"` form.
    /// Error cases:
    /// - not a string
    /// - a string `WeekTime::from_str` rejects
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for Period {
    /// `"2026-11-27 13:00:00..2026-11-27 16:00:00"` form.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.from, self.to)
    }
}

impl Schedule {
    /// Whether the venue is open at `ts`. A timestamp outside chrono's range reads as closed.
    pub fn is_open(&self, ts: Millis) -> bool {
        match self {
            Schedule::AlwaysOpen => true,
            Schedule::Weekly { tz, windows, closed_dates, closures, special_sessions } => {
                let Some(utc) = DateTime::<Utc>::from_timestamp_millis(ts) else {
                    return false;
                };
                let local = utc.with_timezone(tz).naive_local();
                special_sessions.iter().any(|p| p.contains(local))
                    || (regular_open(windows, closed_dates, local) && !closures.iter().any(|p| p.contains(local)))
            }
        }
    }

    /// Whether the venue is open at every instant from `from` to `to`, inclusive. Openness changes
    /// only at a minute boundary, where weekly windows and closed dates turn, or at a dated period's
    /// start or end, so checking the two ends and each such instant between them checks them all.
    /// Each minute between them costs a check, so callers bound the span.
    pub fn open_throughout(&self, from: Millis, to: Millis) -> bool {
        let Schedule::Weekly { tz, closures, special_sessions, .. } = self else { return true };
        if from > to || !self.is_open(from) || !self.is_open(to) {
            return false;
        }
        let minutes = (from.div_euclid(MS_PER_MINUTE) + 1..=to.div_euclid(MS_PER_MINUTE)).map(|m| m * MS_PER_MINUTE);
        let local =
            |ts: Millis| DateTime::<Utc>::from_timestamp_millis(ts).map(|utc| utc.with_timezone(tz).naive_local());
        let (Some(first), Some(last)) = (local(from), local(to)) else { return false };
        let edges = closures
            .iter()
            .chain(special_sessions)
            .flat_map(|p| [p.from, p.to])
            .filter(|edge| first < *edge && *edge <= last)
            .filter_map(|edge| tz.from_local_datetime(&edge).earliest().map(|at| at.timestamp_millis()));
        minutes.chain(edges).all(|at| self.is_open(at))
    }

    /// Whether the schedule lists any dated closure or special session.
    pub fn has_exceptions(&self) -> bool {
        matches!(self, Schedule::Weekly { closures, special_sessions, .. }
            if !closures.is_empty() || !special_sessions.is_empty())
    }

    /// Checks the schedule can ever open, and that every dated exception changes the hours where it
    /// starts, so a mistyped date or time is refused rather than ignored.
    /// Error cases:
    /// - a weekly schedule with no windows
    /// - a window whose open equals its close
    /// - a closure or special session that does not end after it starts
    /// - a closure or special session that starts at a local time the time zone skips (a DST gap)
    /// - a closure that starts while the weekly windows and closed dates already have the venue shut
    /// - a special session that starts while the venue is already open
    pub fn validate(&self) -> Result<(), String> {
        let Schedule::Weekly { tz, windows, closed_dates, closures, special_sessions } = self else {
            return Ok(());
        };
        if windows.is_empty() {
            return Err("weekly schedule has no windows".into());
        }
        if let Some(w) = windows.iter().find(|w| w.open == w.close) {
            return Err(format!("empty window {}-{}", w.open, w.close));
        }
        if let Some(p) = closures.iter().chain(special_sessions).find(|p| p.from >= p.to) {
            return Err(format!("empty period {p}"));
        }
        if let Some(p) =
            closures.iter().chain(special_sessions).find(|p| tz.from_local_datetime(&p.from).earliest().is_none())
        {
            return Err(format!("period {p} starts at a local time {tz} skips"));
        }
        if let Some(p) = closures.iter().find(|p| !regular_open(windows, closed_dates, p.from)) {
            return Err(format!("closure {p} starts while the venue is already shut"));
        }
        let open_at = |t| regular_open(windows, closed_dates, t) && !closures.iter().any(|c| c.contains(t));
        if let Some(p) = special_sessions.iter().find(|p| open_at(p.from)) {
            return Err(format!("special session {p} starts while the venue is already open"));
        }
        Ok(())
    }
}

impl Window {
    /// Whether `t` falls inside this window, wrapping over the week boundary when needed.
    fn contains(&self, t: WeekTime) -> bool {
        if self.open.0 < self.close.0 {
            self.open.0 <= t.0 && t.0 < self.close.0
        } else {
            t.0 >= self.open.0 || t.0 < self.close.0
        }
    }
}

impl Period {
    /// Whether local time `t` falls inside `[from, to)`.
    fn contains(&self, t: NaiveDateTime) -> bool {
        self.from <= t && t < self.to
    }
}

/// Whether local time `t` is inside a weekly window on a date that is not a closed date.
fn regular_open(windows: &[Window], closed_dates: &[NaiveDate], t: NaiveDateTime) -> bool {
    let week_time = WeekTime(t.weekday().num_days_from_monday() * MINUTES_PER_DAY + t.hour() * 60 + t.minute());
    !closed_dates.contains(&t.date()) && windows.iter().any(|w| w.contains(week_time))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(open: &str, close: &str) -> Window {
        Window { open: open.parse().unwrap(), close: close.parse().unwrap() }
    }

    fn ny_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Millis {
        chrono_tz::America::New_York.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
    }

    fn us_equities() -> Schedule {
        Schedule::Weekly {
            tz: chrono_tz::America::New_York,
            windows: DAYS[..5].iter().map(|d| w(&format!("{d} 09:30"), &format!("{d} 16:00"))).collect(),
            closed_dates: vec![NaiveDate::from_ymd_opt(2026, 11, 26).unwrap()],
            closures: vec![],
            special_sessions: vec![],
        }
    }

    #[test]
    fn equity_session_respects_local_time_and_holidays() {
        let s = us_equities();
        // Tue 20 Oct 2026: closed one minute before the open, open until the minute of the close.
        assert!(!s.is_open(ny_ms(2026, 10, 20, 9, 29)));
        assert!(s.is_open(ny_ms(2026, 10, 20, 9, 30)));
        assert!(s.is_open(ny_ms(2026, 10, 20, 15, 59)));
        assert!(!s.is_open(ny_ms(2026, 10, 20, 16, 0)));
        // Saturday.
        assert!(!s.is_open(ny_ms(2026, 10, 24, 12, 0)));
        // Thanksgiving is a listed closure.
        assert!(!s.is_open(ny_ms(2026, 11, 26, 12, 0)));
        // DST ends 1 Nov 2026; 09:30 local is still the open.
        assert!(s.is_open(ny_ms(2026, 11, 2, 9, 30)));
    }

    #[test]
    fn fx_window_wraps_the_week() {
        let s = Schedule::Weekly {
            tz: chrono_tz::America::New_York,
            windows: vec![w("Sun 17:00", "Fri 17:00")],
            closed_dates: vec![],
            closures: vec![],
            special_sessions: vec![],
        };
        assert!(s.is_open(ny_ms(2026, 10, 19, 3, 0))); // Mon
        assert!(!s.is_open(ny_ms(2026, 10, 23, 17, 0))); // Fri close
        assert!(!s.is_open(ny_ms(2026, 10, 24, 12, 0))); // Sat
        assert!(!s.is_open(ny_ms(2026, 10, 25, 16, 59))); // Sun pre-open
        assert!(s.is_open(ny_ms(2026, 10, 25, 17, 0))); // Sun open
    }

    #[test]
    fn out_of_range_timestamps_read_as_closed() {
        assert!(!us_equities().is_open(i64::MIN));
        assert!(!us_equities().is_open(i64::MAX));
    }

    #[test]
    fn validation_rejects_schedules_that_never_open() {
        let empty = Schedule::Weekly {
            tz: chrono_tz::UTC,
            windows: vec![],
            closed_dates: vec![],
            closures: vec![],
            special_sessions: vec![],
        };
        assert!(empty.validate().is_err());
        let zero = Schedule::Weekly {
            tz: chrono_tz::UTC,
            windows: vec![w("Mon 09:00", "Mon 09:00")],
            closed_dates: vec![],
            closures: vec![],
            special_sessions: vec![],
        };
        assert!(zero.validate().is_err());
        assert!(us_equities().validate().is_ok());
    }

    #[test]
    fn unknown_schedule_fields_are_rejected() {
        let typo = serde_json::json!({ "kind": "weekly", "tz": "UTC", "windows": [], "closed_date": [] });
        assert!(serde_json::from_value::<Schedule>(typo).is_err());
        let typo = serde_json::json!({ "kind": "weekly", "tz": "UTC", "windows": [], "closure": [] });
        assert!(serde_json::from_value::<Schedule>(typo).is_err());
        let typo = serde_json::json!({ "from": "2026-11-27T13:00:00", "until": "2026-11-27T16:00:00" });
        assert!(serde_json::from_value::<Period>(typo).is_err());
    }

    #[test]
    fn an_early_close_shortens_only_its_own_session() {
        let s = us_equities_with(vec![period("2026-11-27 13:00", "2026-11-27 16:00")], vec![]);
        assert!(s.is_open(ny_ms(2026, 11, 27, 12, 59)));
        assert!(!s.is_open(ny_ms(2026, 11, 27, 13, 0)));
        assert!(!s.is_open(ny_ms(2026, 11, 27, 15, 59)));
        // The sessions either side keep their hours.
        assert!(s.is_open(ny_ms(2026, 11, 25, 15, 59)));
        assert!(s.is_open(ny_ms(2026, 11, 30, 9, 30)));
        assert!(s.is_open(ny_ms(2026, 11, 30, 15, 59)));
    }

    #[test]
    fn a_closure_spans_midnight_and_the_weekend() {
        // Halts Thu 24 Dec 2026 at 12:15 Chicago and reopens with the regular Sunday session.
        let s = globex(vec![period("2026-12-24 12:15", "2026-12-27 17:00")], vec![]);
        assert!(s.is_open(chicago_ms(2026, 12, 24, 12, 14)));
        assert!(!s.is_open(chicago_ms(2026, 12, 24, 12, 15)));
        // Thursday evening's session, which the windows alone would open, stays shut.
        assert!(!s.is_open(chicago_ms(2026, 12, 24, 17, 0)));
        assert!(!s.is_open(chicago_ms(2026, 12, 25, 10, 0)));
        assert!(s.is_open(chicago_ms(2026, 12, 27, 17, 0)));
        // A week earlier the same hours are open.
        assert!(s.is_open(chicago_ms(2026, 12, 17, 17, 0)));
    }

    #[test]
    fn a_special_session_opens_on_a_weekend_a_closed_date_and_inside_a_closure() {
        let s = us_equities_with(vec![], vec![period("2026-10-24 10:00", "2026-10-24 12:00")]);
        assert!(!s.is_open(ny_ms(2026, 10, 24, 9, 59)));
        assert!(s.is_open(ny_ms(2026, 10, 24, 10, 0)));
        assert!(!s.is_open(ny_ms(2026, 10, 24, 12, 0)));
        // Thanksgiving is a closed date.
        let s = us_equities_with(vec![], vec![period("2026-11-26 10:00", "2026-11-26 12:00")]);
        assert!(s.is_open(ny_ms(2026, 11, 26, 11, 0)));
        assert!(!s.is_open(ny_ms(2026, 11, 26, 12, 0)));
        let s = globex(
            vec![period("2026-12-24 12:15", "2026-12-27 17:00")],
            vec![period("2026-12-25 08:00", "2026-12-25 10:00")],
        );
        assert!(s.is_open(chicago_ms(2026, 12, 25, 9, 0)));
        assert!(!s.is_open(chicago_ms(2026, 12, 25, 10, 0)));
    }

    #[test]
    fn validation_refuses_exceptions_that_change_nothing_where_they_start() {
        let early_close = period("2026-11-27 13:00", "2026-11-27 16:00");
        assert_eq!(us_equities_with(vec![early_close], vec![]).validate(), Ok(()));
        let backwards = period("2026-11-27 16:00", "2026-11-27 13:00");
        let err = us_equities_with(vec![backwards], vec![]).validate().unwrap_err();
        assert!(err.contains("empty period"), "{err}");
        // Saturday, and Thanksgiving, are already shut.
        for shut in [period("2026-11-28 13:00", "2026-11-28 16:00"), period("2026-11-26 13:00", "2026-11-26 16:00")] {
            let err = us_equities_with(vec![shut], vec![]).validate().unwrap_err();
            assert!(err.contains("already shut"), "{err}");
        }
        let err =
            us_equities_with(vec![], vec![period("2026-10-20 10:00", "2026-10-20 11:00")]).validate().unwrap_err();
        assert!(err.contains("already open"), "{err}");
        let empty = period("2026-10-24 10:00", "2026-10-24 10:00");
        assert!(us_equities_with(vec![], vec![empty]).validate().unwrap_err().contains("empty period"));
        // A session inside the hour New York skips when clocks go forward never opens.
        let skipped = period("2027-03-14 02:00", "2027-03-14 02:30");
        let err = us_equities_with(vec![], vec![skipped]).validate().unwrap_err();
        assert!(err.contains("skips"), "{err}");
        // A special session inside a closure starts while the venue is shut.
        let closure = period("2026-12-24 12:15", "2026-12-27 17:00");
        let inside = period("2026-12-25 08:00", "2026-12-25 10:00");
        assert_eq!(globex(vec![closure], vec![inside]).validate(), Ok(()));
    }

    #[test]
    fn has_exceptions_sees_either_kind() {
        let early_close = vec![period("2026-11-27 13:00", "2026-11-27 16:00")];
        let special = vec![period("2026-10-24 10:00", "2026-10-24 12:00")];
        assert!(!Schedule::AlwaysOpen.has_exceptions());
        assert!(!us_equities().has_exceptions());
        assert!(us_equities_with(early_close.clone(), vec![]).has_exceptions());
        assert!(us_equities_with(vec![], special.clone()).has_exceptions());
        assert!(us_equities_with(early_close, special).has_exceptions());
    }

    #[test]
    fn a_schedule_without_exceptions_encodes_as_before_they_existed() {
        // Exactly the keys of the encoding the committed reference logs carry.
        let json = serde_json::to_value(us_equities()).unwrap();
        let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["closed_dates", "kind", "tz", "windows"]);
    }

    #[test]
    fn exceptions_encode_as_local_date_times_and_round_trip() {
        let s = us_equities_with(
            vec![period("2026-11-27 13:00", "2026-11-27 16:00")],
            vec![period("2026-10-24 10:00", "2026-10-24 12:00")],
        );
        let json = serde_json::to_value(&s).unwrap();
        let early_close = serde_json::json!([{ "from": "2026-11-27T13:00:00", "to": "2026-11-27T16:00:00" }]);
        assert_eq!(json["closures"], early_close);
        assert_eq!(json["special_sessions"][0]["from"], "2026-10-24T10:00:00");
        assert_eq!(serde_json::from_value::<Schedule>(json).unwrap(), s);
    }

    #[test]
    fn open_throughout_sees_every_closure_between_two_open_instants() {
        let s = us_equities_with(vec![period("2026-11-27 13:00", "2026-11-27 13:00:30")], vec![]);
        assert!(s.open_throughout(ny_ms(2026, 10, 20, 9, 30), ny_ms(2026, 10, 20, 15, 59)));
        // Across the close, a night and the next open; across a 30-second halt; and backwards.
        assert!(!s.open_throughout(ny_ms(2026, 10, 20, 15, 59), ny_ms(2026, 10, 21, 9, 31)));
        assert!(!s.open_throughout(ny_ms(2026, 11, 27, 12, 59), ny_ms(2026, 11, 27, 13, 1)));
        assert!(!s.open_throughout(ny_ms(2026, 10, 20, 15, 0), ny_ms(2026, 10, 20, 14, 0)));
        assert!(Schedule::AlwaysOpen.open_throughout(0, i64::MAX / 2));
        // A special session on a Saturday is open throughout, and not past its end.
        let s = us_equities_with(vec![], vec![period("2026-10-24 10:00", "2026-10-24 12:00")]);
        assert!(s.open_throughout(ny_ms(2026, 10, 24, 10, 0), ny_ms(2026, 10, 24, 11, 59)));
        assert!(!s.open_throughout(ny_ms(2026, 10, 24, 11, 59), ny_ms(2026, 10, 24, 12, 1)));
    }

    #[test]
    fn week_time_round_trips() {
        let t: WeekTime = "Sun 17:05".parse().unwrap();
        assert_eq!(t.to_string(), "Sun 17:05");
        assert!("Xyz 10:00".parse::<WeekTime>().is_err());
        assert!("Mon 24:00".parse::<WeekTime>().is_err());
    }

    // ── Helpers ──

    /// A local `"YYYY-MM-DD HH:MM[:SS]"` period.
    fn period(from: &str, to: &str) -> Period {
        let at = |s: &str| {
            NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
                .unwrap()
        };
        Period { from: at(from), to: at(to) }
    }

    /// `us_equities` with dated exceptions.
    fn us_equities_with(closures: Vec<Period>, special_sessions: Vec<Period>) -> Schedule {
        let Schedule::Weekly { tz, windows, closed_dates, .. } = us_equities() else {
            unreachable!("US equities keep weekly hours")
        };
        Schedule::Weekly { tz, windows, closed_dates, closures, special_sessions }
    }

    /// Futures hours in Chicago: Sunday 17:00 to Friday 16:00, with a daily 16:00-17:00 break.
    fn globex(closures: Vec<Period>, special_sessions: Vec<Period>) -> Schedule {
        let windows = ["Sun", "Mon", "Tue", "Wed", "Thu"]
            .iter()
            .zip(&DAYS[..5])
            .map(|(open, close)| w(&format!("{open} 17:00"), &format!("{close} 16:00")));
        Schedule::Weekly {
            tz: chrono_tz::America::Chicago,
            windows: windows.collect(),
            closed_dates: vec![],
            closures,
            special_sessions,
        }
    }

    fn chicago_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Millis {
        chrono_tz::America::Chicago.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
    }
}
