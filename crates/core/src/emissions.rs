//! MONKEY emissions, from protocol `V19`: fixed ten-minute budgets shared pro rata by eligible
//! realised losses, halving every 26,280 periods (182.5 days), from the start the operator's signed
//! initialisation records.
//!
//! - **Periods** are the half-open windows `[start + n·PERIOD_MS, start + (n+1)·PERIOD_MS)` of
//!   sequenced time. A command stamped exactly at a boundary belongs to the later period.
//! - **Weight.** An account's weight in a period is its net realised trading loss there, the sum of
//!   its fills' `realised_pnl` (liquidations included, the profit fee and financing not) and, from
//!   `V23`, of the cash in lieu splits realise on its positions (dividends not), less the debt the
//!   house wrote off for it there. From `V32`, the write-off a period's losses cannot absorb
//!   becomes a per-account emission debt, and later loss weight pays the debt down before any of it
//!   earns weight again. A weight that is not positive counts as zero, and so does any weight of an
//!   account whose cash is negative at the period's cutoff, with no later backfill.
//! - **Budget.** Period `n`'s budget is the initial budget halved, rounded down, once for each whole
//!   26,280 periods before `n`; once it reaches zero it stays zero. The nominal maximum supply is
//!   `2 · 26,280 ·` the initial budget, and every budget and allocation rounds below it.
//! - **Allocation.** Each account with weight receives `floor(budget · weight / total weight)` base
//!   units. The remainder, and the whole budget of a period with no weight, is cancelled for good:
//!   nothing carries forward or is minted later.
//! - **Finality.** The first command at or after a period's end finalises it before it does
//!   anything else, so that command, and every effect it has, belongs to the next period. Periods
//!   no command fell in are cancelled in one step, however many there are.
//! - **Entitlements** are cumulative per account and only grow; each has a leaf of its own in the
//!   state tree, which the vault checks a claim against.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use alloy_primitives::U256;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Fixed;
use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MAX_STAMP_MS, Millis};

/// The length of one emission period: ten minutes.
pub const PERIOD_MS: Millis = 600_000;
/// Periods between halvings: 182.5 days of ten-minute periods.
pub const PERIODS_PER_HALVING: u64 = 26_280;
/// The largest initial period budget, in base units: the one whose nominal maximum supply,
/// `2 · PERIODS_PER_HALVING ·` the budget, still fits a `u128`.
pub const MAX_PERIOD_BUDGET: u128 = u128::MAX / (2 * PERIODS_PER_HALVING as u128);

/// An amount of MONKEY in token base units. Serializes as a decimal string, so no JSON client
/// loses precision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct TokenUnits(pub u128);

/// The schedule the operator's initialisation records once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmissionSchedule {
    /// When period 0 starts, in sequenced epoch milliseconds. Never earlier than the
    /// initialisation's own stamp.
    pub start_ms: Millis,
    /// Period 0's budget, in base units; later budgets halve from it.
    pub period_budget: TokenUnits,
}

/// Emissions once initialised: the schedule, the open period, and what finalised periods did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Emissions {
    pub schedule: EmissionSchedule,
    /// The period losses accrue to: the one the last command fell in, or 0 before the start.
    pub period: u64,
    /// Base units allocated to accounts across every finalised period.
    pub entitled: TokenUnits,
    /// Base units cancelled across every finalised period: empty budgets and allocation remainders.
    pub cancelled: TokenUnits,
}

/// An account's realised trading result in the open period.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeriodResult {
    /// The sum of its fills' realised PnL in USD, liquidations included, before the profit fee, and
    /// from `V23` of the cash in lieu its positions' splits realise.
    pub realised: Fixed,
    /// The debt the house wrote off for it.
    pub written_off: Fixed,
}

impl fmt::Display for TokenUnits {
    /// The amount in base units, in decimal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for TokenUnits {
    type Err = String;

    /// Parses decimal digits only: no sign, no leading zero, nothing past `u128::MAX`.
    /// Error cases:
    /// - empty, a character other than a digit, a leading zero, or a value past `u128::MAX`
    fn from_str(s: &str) -> Result<Self, String> {
        let canonical = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
        canonical
            .then(|| s.parse().ok())
            .flatten()
            .map(TokenUnits)
            .ok_or_else(|| format!("invalid token amount {s:?}: expected base units in decimal digits"))
    }
}

impl Serialize for TokenUnits {
    /// As a decimal string.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TokenUnits {
    /// From a decimal string; see `FromStr`.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
    }
}

impl EmissionSchedule {
    /// The period `ts` falls in, or `None` before the start.
    pub fn period_at(&self, ts: Millis) -> Option<u64> {
        // Both stamps are at most `MAX_STAMP_MS`, so the difference fits.
        let elapsed = ts.checked_sub(self.start_ms).filter(|elapsed| *elapsed >= 0)?;
        Some((elapsed / PERIOD_MS) as u64)
    }

    /// When `period` ends, and the next begins.
    pub fn period_end(&self, period: u64) -> Millis {
        // Periods are only ever counted up to the latest stamp, so the end fits a `Millis`.
        self.start_ms + (period as Millis + 1) * PERIOD_MS
    }

    /// `period`'s budget in base units: the initial budget halved once per whole
    /// `PERIODS_PER_HALVING` before it, rounded down, and zero from the 128th halving on.
    pub fn budget(&self, period: u64) -> u128 {
        u32::try_from(period / PERIODS_PER_HALVING)
            .ok()
            .and_then(|halvings| self.period_budget.0.checked_shr(halvings))
            .unwrap_or(0)
    }

    /// The sum of the budgets of the periods from `first` up to, not including, `end`, one step per
    /// halving the range spans.
    pub fn budgets(&self, first: u64, end: u64) -> u128 {
        let mut total = 0;
        let mut period = first;
        while period < end {
            let budget = self.budget(period);
            if budget == 0 {
                break;
            }
            let next = (period / PERIODS_PER_HALVING + 1).saturating_mul(PERIODS_PER_HALVING).min(end);
            // Every budget sum is at most the nominal supply, which `MAX_PERIOD_BUDGET` keeps in a `u128`.
            total += budget * u128::from(next - period);
            period = next;
        }
        total
    }

    /// The nominal maximum supply: `2 · PERIODS_PER_HALVING ·` the initial budget, the ceiling every
    /// scheduled budget sums below.
    pub fn nominal_supply(&self) -> u128 {
        2 * u128::from(PERIODS_PER_HALVING) * self.period_budget.0
    }

    /// Checks the schedule against the time it is initialised at.
    /// Error cases:
    /// - a start before `now`, or past the latest stamp the engine accepts
    /// - a zero budget, or one above `MAX_PERIOD_BUDGET`
    fn validate(&self, now: Millis) -> Result<(), String> {
        if self.start_ms < now {
            return Err(format!("emissions start {} is before the sequenced time {now}", self.start_ms));
        }
        if self.start_ms > MAX_STAMP_MS {
            return Err(format!("emissions start {} is past the latest stamp", self.start_ms));
        }
        if self.period_budget.0 == 0 || self.period_budget.0 > MAX_PERIOD_BUDGET {
            return Err(format!("period budget {} is outside 1 to {MAX_PERIOD_BUDGET}", self.period_budget));
        }
        Ok(())
    }
}

impl PeriodResult {
    /// The account's weight: its net realised loss less what the house wrote off, or zero.
    fn weight(&self) -> Fixed {
        (-self.realised - self.written_off).max(Fixed::ZERO)
    }
}

impl Engine {
    /// Emissions, once initialised.
    pub fn emissions(&self) -> Option<&Emissions> {
        self.emissions.as_ref()
    }

    /// Each account's realised result in the open period.
    pub fn period_results(&self) -> &BTreeMap<AccountId, PeriodResult> {
        &self.period_results
    }

    /// `account`'s cumulative entitlement in base units.
    pub fn entitlement(&self, account: AccountId) -> TokenUnits {
        self.entitlements.get(&account).copied().unwrap_or_default()
    }

    /// `account`'s unabsorbed emission write-off debt from `V32`: later loss weight pays it down
    /// before the account earns emission weight again.
    pub fn emission_debt(&self, account: AccountId) -> Fixed {
        self.emission_debts.get(&account).copied().unwrap_or_default()
    }

    /// Records the schedule, once, under `V19` or later.
    /// Error cases:
    /// - before `V19`, or once emissions are initialised
    /// - a schedule that fails its checks at the sequenced time (see `EmissionSchedule::validate`)
    pub(crate) fn init_emissions(&mut self, schedule: &EmissionSchedule) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V19 {
            return Err(format!("emissions need protocol {:?}", ProtocolVersion::V19));
        }
        if self.emissions.is_some() {
            return Err("emissions are already initialised".into());
        }
        schedule.validate(self.now)?;
        self.emissions = Some(Emissions {
            schedule: *schedule,
            period: 0,
            entitled: TokenUnits::default(),
            cancelled: TokenUnits::default(),
        });
        Ok(())
    }

    /// Adds a fill's realised PnL and any debt written off to `account`'s result in the open period,
    /// once emissions have started. From `V32` that is judged at the enclosing command's stamp, not
    /// at the boundary a corporate catch-up has rewound the clock to.
    pub(crate) fn record_period_result(&mut self, account: AccountId, realised: Fixed, written_off: Fixed) {
        let at = if self.protocol_version >= ProtocolVersion::V32 { self.command_ms } else { self.now };
        let started = self.emissions.as_ref().is_some_and(|emissions| at >= emissions.schedule.start_ms);
        if started && !(realised.is_zero() && written_off.is_zero()) {
            let result = self.period_results.entry(account).or_default();
            result.realised += realised;
            result.written_off += written_off;
        }
    }

    /// From `V32`: the part of `result`'s write-off that its realised losses cannot absorb becomes
    /// `account`'s emission debt, and `weight` (zero for negative cash at the cutoff) pays the debt
    /// down before any of it earns: the weight spent on the debt earns nothing. Returns the weight
    /// left to earn this period. The debt is not capped at what the account was ever allocated.
    fn settle_emission_debt(&mut self, account: AccountId, result: &PeriodResult, weight: Fixed) -> Fixed {
        let losses = (-result.realised).max(Fixed::ZERO);
        let unabsorbed = (result.written_off - losses).max(Fixed::ZERO);
        let debt = self.emission_debts.remove(&account).unwrap_or_default() + unabsorbed;
        let (earning, remaining) =
            if weight >= debt { (weight - debt, Fixed::ZERO) } else { (Fixed::ZERO, debt - weight) };
        if !remaining.is_zero() {
            self.emission_debts.insert(account, remaining);
        }
        earning
    }

    /// Finalises the open period once the sequenced time has reached its end: allocates its budget
    /// to the accounts with weight at their cash now, cancels the remainder, and cancels every later
    /// period the time has passed with no command. Runs before anything else the command does.
    pub(crate) fn finalise_emissions(&mut self, events: &mut Vec<Event>) {
        let Some(emissions) = &self.emissions else { return };
        let (schedule, open) = (emissions.schedule, emissions.period);
        let Some(current) = schedule.period_at(self.now).filter(|current| *current > open) else { return };

        let budget = schedule.budget(open);
        let v32 = self.protocol_version >= ProtocolVersion::V32;
        let mut weights: Vec<(AccountId, Fixed)> = Vec::new();
        for (account, result) in self.period_results.take_all() {
            let negative_cash = self.accounts.get(&account).is_some_and(|acct| acct.balance.is_negative());
            // Weight forfeited for negative cash pays no debt, while a new write-off still adds to it.
            let weight = if negative_cash { Fixed::ZERO } else { result.weight() };
            let weight = if v32 { self.settle_emission_debt(account, &result, weight) } else { weight };
            if weight.is_positive() {
                weights.push((account, weight));
            }
        }
        // Weights are positive `i128`s, so their sum fits 2^160 and its product with a budget 2^256.
        let units = |weight: Fixed| U256::from(weight.raw().unsigned_abs());
        let total: U256 = weights.iter().map(|(_, weight)| units(*weight)).sum();
        let mut allocated = 0;
        for (account, weight) in weights {
            // At most `budget`, as each weight is at most the total.
            let amount: u128 = (U256::from(budget) * units(weight) / total).to();
            let entitled = self.entitlements.entry(account).or_default();
            entitled.0 += amount;
            allocated += amount;
            events.push(Event::EmissionAllocated { account, period: open, weight, amount: TokenUnits(amount) });
        }
        let dust = budget - allocated;
        let skipped = schedule.budgets(open + 1, current);
        events.push(Event::EmissionPeriodClosed {
            period: open,
            budget: TokenUnits(budget),
            allocated: TokenUnits(allocated),
            cancelled: TokenUnits(dust),
        });
        if current > open + 1 {
            events.push(Event::EmissionPeriodsCancelled {
                first: open + 1,
                last: current - 1,
                cancelled: TokenUnits(skipped),
            });
        }
        let emissions = self.emissions.as_mut().expect("read above");
        emissions.period = current;
        emissions.entitled.0 += allocated;
        emissions.cancelled.0 += dust + skipped;
    }
}
