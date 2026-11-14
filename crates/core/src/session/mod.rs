//! Trading hours. Crypto trades 24/7; RWA markets follow their venue's weekly windows in local
//! time (so DST is handled by the tz database), minus holiday closures and dated closures such as
//! early closes, plus dated special sessions. While a market is closed nothing fills. Resting
//! orders wait, and the first fresh price after the open fills them, gap included.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

mod weekend;
pub use weekend::*;

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
mod tests;
