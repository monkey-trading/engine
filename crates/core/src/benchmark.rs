//! Dated benchmark inputs and financing lookup from V18. Published rates are annual fractions,
//! preserving sub-basis-point precision; the signed batch is installed before catch-up financing.
//!
//! From V46 a signed rate moves at most `MAX_BENCHMARK_MOVE_BPS_PER_DAY` per calendar day between
//! its date and each applied observation next to it, so an elected operator cannot sign an outlying
//! rate towards the NAV its electors own. A refused rate changes nothing: the last applied rate
//! keeps financing, and since the limit grows with the days from it, a real move lands once enough
//! days have passed. The operator's worker removes what the cap refuses before it signs
//! (`split_by_cap`), so one refusal does not hold back the other benchmarks or a rollover waiting
//! for a snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Bound;

use chrono::{DateTime, Datelike, NaiveDate, Weekday};
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;
use crate::types::{MarketId, Millis};

/// At most this many observations in one signed snapshot.
pub const MAX_BENCHMARK_RATES: usize = 16_384;
pub type BenchmarkHistory = BTreeMap<String, BTreeMap<NaiveDate, Fixed>>;

/// From V46, the most a benchmark rate may move from an applied rate, per calendar day between their
/// dates. A constant, not a parameter, so the operator it bounds cannot change it.
pub const MAX_BENCHMARK_MOVE_BPS_PER_DAY: i64 = 300;
// Every offset in pinned chrono-tz 2025b is east of -18h (the minimum is -15:56:08).
const MAX_WESTWARD_OFFSET_MS: Millis = 18 * 60 * 60 * 1_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkRate {
    pub benchmark: String,
    pub effective_date: NaiveDate,
    /// Annual fraction: 0.045 means 4.5%, or 450 bps.
    pub annual_rate: Fixed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkBatch {
    /// The rollover-local date through which the worker attempted to fetch publisher histories.
    pub as_of: NaiveDate,
    /// Includes failed fetches: their previous rate still applies, or no financing if none exists.
    pub checked: BTreeSet<String>,
    pub rates: Vec<BenchmarkRate>,
}

/// A rate the V46 cap refuses, and the applied rate it is measured from: the date is `None` for a
/// benchmark's undated rate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefusedBenchmarkRate {
    pub rate: BenchmarkRate,
    pub from_date: Option<NaiveDate>,
    pub from_rate: Fixed,
}

impl fmt::Display for RefusedBenchmarkRate {
    /// The refusal as it reads in an `AdminRejected` reason or an alert.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rate = &self.rate;
        write!(
            f,
            "benchmark {} rate {} on {} moves more than {MAX_BENCHMARK_MOVE_BPS_PER_DAY} bps a day from {}",
            rate.benchmark, rate.annual_rate, rate.effective_date, self.from_rate
        )?;
        match self.from_date {
            Some(date) => write!(f, " on {date}"),
            None => write!(f, " undated"),
        }
    }
}

impl Engine {
    /// The rollover-local calendar date at `ts`, or UTC before parameters are recorded.
    pub fn benchmark_date(&self, ts: Millis) -> NaiveDate {
        let utc = DateTime::from_timestamp_millis(ts).expect("engine timestamps fit chrono");
        self.params.as_ref().map_or(utc.date_naive(), |p| utc.with_timezone(&p.rollover.tz).date_naive())
    }

    /// The next rollover, for a publisher worker to request history covering downtime.
    pub fn next_rollover(&self) -> Option<Millis> {
        self.next_rollover
    }

    /// Publisher history retained for unpaid rollovers and future legal calendar changes.
    pub fn benchmark_history(&self) -> &BenchmarkHistory {
        &self.benchmark_history
    }

    /// Validates the whole snapshot before changing any rate. An empty history records a failed
    /// fetch attempt and deliberately retains the missing/stale financing policy.
    /// Error cases:
    /// - before V18, wrong as-of date, too many rates, an unchecked market, or invalid/duplicate data
    /// - from V46, a rate the cap refuses (see `split_by_cap`)
    pub(crate) fn set_benchmark_rates(&mut self, batch: &BenchmarkBatch) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V18 {
            return Err("dated benchmark rates need protocol V18".into());
        }
        if batch.as_of != self.benchmark_date(self.now) {
            return Err("benchmark snapshot must cover the current rollover-local date".into());
        }
        if batch.rates.len() > MAX_BENCHMARK_RATES || batch.checked.len() > 256 {
            return Err("benchmark snapshot is too large".into());
        }
        if self.markets.values().any(|m| !batch.checked.contains(&m.spec.financing.benchmark)) {
            return Err("benchmark snapshot must check every listed market's benchmark".into());
        }
        let mut seen = BTreeSet::new();
        for rate in &batch.rates {
            if rate.benchmark.is_empty() || rate.benchmark.len() > 64 || !batch.checked.contains(&rate.benchmark) {
                return Err("invalid or unchecked benchmark name".into());
            }
            if rate.annual_rate < -Fixed::ONE || rate.annual_rate > Fixed::ONE {
                return Err("benchmark annual rate must be within +/-100%".into());
            }
            if rate.effective_date > batch.as_of || rate.effective_date.year() < 1970 {
                return Err("benchmark effective date must be between 1970 and the snapshot date".into());
            }
            if !seen.insert((&rate.benchmark, rate.effective_date)) {
                return Err("duplicate benchmark effective date".into());
            }
        }
        if self.protocol_version >= ProtocolVersion::V46
            && let Some(refused) = split_by_cap(&self.benchmark_history, &self.benchmarks, &batch.rates).1.first()
        {
            return Err(refused.to_string());
        }
        for rate in &batch.rates {
            self.benchmark_history
                .entry(rate.benchmark.clone())
                .or_default()
                .insert(rate.effective_date, rate.annual_rate);
        }
        if let Some(at) = self.next_rollover {
            self.prune_benchmark_history(self.benchmark_date(at));
        }
        Ok(())
    }

    /// From V46, checks a signed undated rate as an observation dated today, as `SetBenchmarkRate`
    /// records it.
    /// Error cases:
    /// - a rate the cap refuses (see `split_by_cap`)
    pub(crate) fn check_undated_benchmark_rate(&self, benchmark: &str, annual_bps: i64) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V46 {
            return Ok(());
        }
        let rate = BenchmarkRate {
            benchmark: benchmark.to_owned(),
            effective_date: self.benchmark_date(self.now),
            annual_rate: Fixed::from_bps(annual_bps),
        };
        match split_by_cap(&self.benchmark_history, &self.benchmarks, &[rate]).1.first() {
            Some(refused) => Err(refused.to_string()),
            None => Ok(()),
        }
    }

    /// The last published observation effective on or before a rollover's local date.
    pub(crate) fn dated_benchmark(&self, name: &str, date: NaiveDate) -> Option<(NaiveDate, Fixed)> {
        self.benchmark_history.get(name)?.range(..=date).next_back().map(|(day, rate)| (*day, *rate))
    }

    /// Records missing and stale observations, and their recovery, for every required benchmark.
    pub(crate) fn benchmark_status(&self, date: NaiveDate, events: &mut Vec<Event>) {
        let mut markets = BTreeMap::<&str, Vec<MarketId>>::new();
        for (id, market) in self.markets.iter() {
            markets.entry(&market.spec.financing.benchmark).or_default().push(*id);
        }
        for (benchmark, markets) in markets {
            match self.dated_benchmark(benchmark, date) {
                Some((effective_date, _)) => events.push(Event::BenchmarkRateObserved {
                    benchmark: benchmark.into(),
                    effective_date,
                    rollover_date: date,
                    stale: business_days(effective_date, date) > 5,
                }),
                None if self.benchmarks.contains_key(benchmark) => {
                    events.push(Event::BenchmarkRateUndated { benchmark: benchmark.into() })
                }
                None => events.push(Event::BenchmarkRateMissing { benchmark: benchmark.into(), markets }),
            }
        }
    }

    /// Keeps predecessors for unpaid rollovers and any local date a future calendar can select.
    pub(crate) fn prune_benchmark_history(&mut self, date: NaiveDate) {
        // Monotone engine time and the offset bound protect future calendars; `date` protects catch-up.
        let earliest_future_date = DateTime::from_timestamp_millis(self.now - MAX_WESTWARD_OFFSET_MS)
            .expect("engine timestamps fit chrono")
            .date_naive();
        let date = date.min(earliest_future_date);
        for history in self.benchmark_history.values_mut() {
            if let Some((&last, _)) = history.range(..=date).next_back() {
                *history = history.split_off(&last);
            }
        }
    }
}

/// Splits `rates` into those the V46 cap admits and those it refuses, taking each benchmark's rates
/// in date order and measuring each from the observations next to its date, among `history` and
/// what it has admitted so far: the limit is the cap times the days between them, in each direction.
/// An applied date does not change: the rate it holds passes, as the worker sends it again, and any
/// other is refused, since a change measured from its neighbours could be ratcheted, or walked from a
/// backdated rate, past the cap. A benchmark's undated rate stands in for the observation before a rate
/// that has no dated one, as one day away. A rate with no neighbour at all is the benchmark's first,
/// bound only by the ±100% range.
pub fn split_by_cap(
    history: &BenchmarkHistory,
    undated: &BTreeMap<String, i64>,
    rates: &[BenchmarkRate],
) -> (Vec<BenchmarkRate>, Vec<RefusedBenchmarkRate>) {
    let mut sorted: Vec<&BenchmarkRate> = rates.iter().collect();
    sorted.sort_by(|a, b| (&a.benchmark, a.effective_date).cmp(&(&b.benchmark, b.effective_date)));
    let mut applied: BTreeMap<&str, BTreeMap<NaiveDate, Fixed>> = BTreeMap::new();
    let (mut admitted, mut refused) = (Vec::new(), Vec::new());
    for rate in sorted {
        let observed =
            applied.entry(&rate.benchmark).or_insert_with(|| history.get(&rate.benchmark).cloned().unwrap_or_default());
        match refusal(rate, observed, undated.get(&rate.benchmark).copied()) {
            Some(refusal) => refused.push(refusal),
            None => {
                observed.insert(rate.effective_date, rate.annual_rate);
                admitted.push(rate.clone());
            }
        }
    }
    (admitted, refused)
}

/// Why the cap refuses `rate` against the observations `observed`, if it does.
fn refusal(
    rate: &BenchmarkRate,
    observed: &BTreeMap<NaiveDate, Fixed>,
    undated_bps: Option<i64>,
) -> Option<RefusedBenchmarkRate> {
    let date = rate.effective_date;
    let refused = |from_date, from_rate| RefusedBenchmarkRate { rate: rate.clone(), from_date, from_rate };
    if let Some(applied) = observed.get(&date) {
        return (*applied != rate.annual_rate).then(|| refused(Some(date), *applied));
    }
    let before = observed
        .range(..date)
        .next_back()
        .map(|(day, value)| (Some(*day), *value, (date - *day).num_days()))
        .or_else(|| undated_bps.map(|bps| (None, Fixed::from_bps(bps), 1)));
    if let Some((day, value, days)) = before
        && !within_cap(rate.annual_rate, value, days)
    {
        return Some(refused(day, value));
    }
    let after = observed.range((Bound::Excluded(date), Bound::Unbounded)).next();
    after.and_then(|(day, value)| {
        (!within_cap(*value, rate.annual_rate, (*day - date).num_days())).then(|| refused(Some(*day), *value))
    })
}

/// Whether two rates `days` apart are within the V46 cap of each other.
fn within_cap(rate: Fixed, from: Fixed, days: i64) -> bool {
    (rate - from).abs() <= Fixed::from_bps(MAX_BENCHMARK_MOVE_BPS_PER_DAY.saturating_mul(days))
}

/// Weekdays strictly after `from`, through `to`; publisher holidays do not advance observations.
pub fn business_days(from: NaiveDate, to: NaiveDate) -> i64 {
    let days = (to - from).num_days().max(0);
    let weeks = days / 7;
    let remaining = (1..=days % 7)
        .filter(|offset| !matches!((from + chrono::Duration::days(*offset)).weekday(), Weekday::Sat | Weekday::Sun))
        .count();
    weeks * 5 + remaining as i64
}

/// The state the operator worker reads before fetching a restart's missing history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkState {
    pub next_rollover: Option<Millis>,
    pub rollover: Option<crate::params::Rollover>,
    pub required: BTreeSet<String>,
    pub history: BenchmarkHistory,
    /// Each benchmark's undated rate, in bps a year, which the V46 cap measures a first dated rate from.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub undated: BTreeMap<String, i64>,
}

impl Engine {
    /// The public benchmark schedule and retained observations.
    pub fn benchmark_state(&self) -> BenchmarkState {
        BenchmarkState {
            next_rollover: self.next_rollover,
            rollover: self.params.as_ref().map(|p| p.rollover.clone()),
            required: self.markets.values().map(|m| m.spec.financing.benchmark.clone()).collect(),
            history: self.benchmark_history.clone(),
            undated: self.benchmarks.clone(),
        }
    }
}

impl BenchmarkState {
    /// The rollover-local date at `ts`.
    pub fn date_at(&self, ts: Millis) -> NaiveDate {
        let utc = DateTime::from_timestamp_millis(ts).expect("timestamps fit chrono");
        self.rollover.as_ref().map_or(utc.date_naive(), |r| utc.with_timezone(&r.tz).date_naive())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn timezone_database_changes_require_reauditing_the_retention_offset_bound() {
        assert_eq!(chrono_tz::IANA_TZDB_VERSION, "2025b", "re-audit every UTC offset against the retention bound");
    }
}
