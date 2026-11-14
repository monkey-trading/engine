//! The profit fee that falls with holding time, from `V55`.
//!
//! A realised gain pays the larger of the base fee (`ProtocolParams::profit_fee_bps`) and the hold
//! curve's fee at the hold of the slice it closes, `max(base, C(hold))`; a loss pays nothing. The
//! curve (`ProtocolParams::profit_fee_curve`) is a table of points, ascending in hold and falling in
//! fee: the first point's fee at and below its hold, linear between points with the fee rounded up
//! to the bp, and zero at and past the last point's hold, where the base alone applies.
//!
//! A hold runs from the position's open time (`Account::opened_at`). Opening a position sets it to
//! the fill's sequenced time; an add moves it to the size-weighted average of now and the open time
//! raised to no earlier than now less the curve's last hold, rounded towards now, so an add does not
//! restart the clock and the units it adds never look older than the curve's end. From `V57` that
//! clamp is the launch curve's last hold, whatever curve is in force, so a later curve that reaches
//! further cannot widen the age an add borrows; a log launched with no curve has no hold to pin, and
//! clamps at the curve in force's. A reduction leaves the open time; a flip closes the
//! whole position at its hold and opens the rest now. A close-out is a close, and a split keeps the
//! open time. A hold runs to the command's stamp, also for a close-out a catch-up runs at an earlier
//! boundary. Before `V55` a curve in the parameters charges nothing; activation raises a base cut
//! below its launch value back to it, gives parameters that name no curve the default one, and gives
//! each held position the open time `activation - the curve's last hold`, so its next close pays the
//! base. `V57`'s activation keeps every open time.
//!
//! The operator moves the base only from its launch value up to +50%, and the curve only to one at or
//! above the launch curve at every hold, reaching at most 30 minutes, each after a seven-day notice
//! (`operator_bounds`), so no hold pays less than at launch.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::Engine;
use crate::fixed::Rounding;
use crate::operator_bounds::ParamsNotice;
use crate::params::MAX_RATE_BPS;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId, Millis};

/// The most points a curve may hold, which bounds the work a close's fee takes.
pub const MAX_CURVE_POINTS: usize = 32;
/// The longest hold a curve may reach: 30 minutes.
pub const MAX_CURVE_HOLD_MS: Millis = 30 * 60 * 1_000;

/// The curve parameters set before `V55` take at its activation: the full fee on a gain held up to
/// 10 s, falling to 10% at 15 minutes, linear in the logarithm of the hold between them. Eleven
/// holds spaced evenly in that logarithm, each rounded up to the millisecond, with the curve's fee
/// there rounded up to the bp, so the table, linear between them, is never below that curve and at
/// most 0.52 percentage points above it.
pub const DEFAULT_PROFIT_FEE_CURVE: [CurvePoint; 11] = [
    CurvePoint { hold_ms: 10_000, fee_bps: 10_000 },
    CurvePoint { hold_ms: 15_683, fee_bps: 9_100 },
    CurvePoint { hold_ms: 24_596, fee_bps: 8_200 },
    CurvePoint { hold_ms: 38_573, fee_bps: 7_300 },
    CurvePoint { hold_ms: 60_492, fee_bps: 6_400 },
    CurvePoint { hold_ms: 94_869, fee_bps: 5_500 },
    CurvePoint { hold_ms: 148_781, fee_bps: 4_600 },
    CurvePoint { hold_ms: 233_330, fee_bps: 3_700 },
    CurvePoint { hold_ms: 365_927, fee_bps: 2_800 },
    CurvePoint { hold_ms: 573_877, fee_bps: 1_900 },
    CurvePoint { hold_ms: 900_000, fee_bps: 1_000 },
];

/// One point of the hold curve: the fee, in bps of the gain, on a slice held `hold_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurvePoint {
    pub hold_ms: Millis,
    pub fee_bps: i64,
}

/// What closing a position would pay now: when it opened, the fee rate on a gain, and when that rate
/// next falls (`None` once it is the base).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CloseFee {
    pub opened_at: Millis,
    pub fee_bps: i64,
    pub falls_at: Option<Millis>,
}

impl Engine {
    /// The hold of `account`'s position in `market` and the fee rate a close of it pays now: from
    /// `V55` the time since its open time and `max(base, C(hold))`, and before `V55`, or with no
    /// position, no hold and the base.
    pub(crate) fn close_fee_terms(&self, account: AccountId, market: MarketId) -> (Option<Millis>, i64) {
        let params = self.params.as_ref().expect("callers check params are set");
        let held = self.accounts.get(&account).filter(|acct| !acct.position(market).qty.is_zero());
        let Some(acct) = held.filter(|_| self.protocol_version >= ProtocolVersion::V55) else {
            return (None, params.profit_fee_bps);
        };
        // Activation, opening and every flip set the open time of a position they leave held.
        let opened_at = acct.opened_at.get(&market).expect("from V55 every position holds an open time");
        let hold = self.hold_clock_time() - opened_at;
        (Some(hold), fee_at(params.profit_fee_bps, &params.profit_fee_curve, hold))
    }

    /// The time a hold runs to and an open time is set at: the command's own stamp, also while a
    /// catch-up has rewound the clock to an earlier boundary (as emissions judge a fill from `V32`),
    /// so a close-out there never measures a hold back past its open; outside a command, the last
    /// sequenced time.
    pub(crate) fn hold_clock_time(&self) -> Millis {
        self.now.max(self.command_ms)
    }

    /// The most age the units a position holds lend an add (`opened_after`): from `V57` the launch
    /// curve's last hold, so a later curve cannot widen it, and before it, or while the log launched
    /// with no curve, the last hold of the curve in force.
    pub(crate) fn add_clamp_hold(&self) -> Millis {
        let params = self.params.as_ref().expect("callers check params are set");
        if self.protocol_version < ProtocolVersion::V57 {
            return last_hold(&params.profit_fee_curve);
        }
        // Recorded with the first parameters under `V55`, or at its activation over them.
        let launch =
            self.operator_bounds.launch_curve.as_deref().expect("the launch curve is recorded with parameters");
        // A log launched with no curve has no hold to pin, and its bounds admit any curve later, so
        // pinning a zero hold would only restart the clock of every position an add touches.
        if launch.is_empty() { last_hold(&params.profit_fee_curve) } else { last_hold(launch) }
    }

    /// What closing `account`'s position in `market` would pay now, from `V55`; `None` with no
    /// position, or before `V55`.
    pub fn close_fee(&self, account: AccountId, market: MarketId) -> Option<CloseFee> {
        let (Some(hold), fee_bps) = self.close_fee_terms(account, market) else { return None };
        let params = self.params.as_ref().expect("a position needs params");
        let opened_at = self.hold_clock_time() - hold;
        let falls = next_fall(params.profit_fee_bps, &params.profit_fee_curve, hold);
        Some(CloseFee { opened_at, fee_bps, falls_at: falls.map(|after| opened_at + after) })
    }

    /// `close_fee` for each of `account`'s positions, by market; empty before `V55`.
    pub fn close_fees(&self, account: AccountId) -> BTreeMap<MarketId, CloseFee> {
        let Some(acct) = self.accounts.get(&account) else { return BTreeMap::new() };
        acct.positions.keys().filter_map(|market| self.close_fee(account, *market).map(|fee| (*market, fee))).collect()
    }

    /// From `V55`, checks a base profit fee is not below its launch value: past the curve the base is
    /// the calibrated fee, so a cut would lower what every long hold pays.
    /// Error cases:
    /// - from `V55`, `fee_bps` below `launch_bps`
    pub(crate) fn check_fee_floor(&self, fee_bps: i64, launch_bps: i64) -> Result<(), String> {
        if self.protocol_version >= ProtocolVersion::V55 && fee_bps < launch_bps {
            return Err(format!("profit_fee_bps {fee_bps} is below its launch value {launch_bps}"));
        }
        Ok(())
    }

    /// Checks a curve a notice or parameters name is at or above the launch curve at every hold,
    /// before rounding.
    /// Error cases:
    /// - before `V55`
    /// - a curve below the launch curve at some hold, or one that reaches less far
    pub(crate) fn check_curve_bound(&self, curve: &[CurvePoint]) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V55 {
            return Err(format!("a profit fee curve needs protocol {:?}", ProtocolVersion::V55));
        }
        // Recorded with the first parameters under `V55`, or at its activation over them.
        let launch =
            self.operator_bounds.launch_curve.as_deref().expect("the launch curve is recorded with parameters");
        match first_hold_below(curve, launch) {
            Some(hold) => Err(format!("profit_fee_curve is below its launch curve at a hold of {hold} ms")),
            None => Ok(()),
        }
    }

    /// Checks the curve a notice names, if any: its shape (`validate_curve`) and its bound
    /// (`check_curve_bound`).
    /// Error cases:
    /// - a curve `validate_curve` or `check_curve_bound` refuses
    pub(crate) fn check_curve_notice(&self, curve: Option<&[CurvePoint]>) -> Result<(), String> {
        let Some(curve) = curve else { return Ok(()) };
        validate_curve(curve)?;
        self.check_curve_bound(curve)
    }

    /// From `V55`, records the curve in force as the launch curve with the first parameters set under
    /// it; its activation over parameters already set records it there (`start_hold_clocks`).
    pub(crate) fn record_launch_curve(&mut self) {
        if let (Some(params), None) = (&self.params, &self.operator_bounds.launch_curve)
            && self.protocol_version >= ProtocolVersion::V55
        {
            self.operator_bounds.launch_curve = Some(params.profit_fee_curve.clone());
        }
    }

    /// From `V55`, checks parameters change the curve only to one within its bound
    /// (`check_curve_bound`), named by a notice whose time has come. Before `V55` a curve charges
    /// nothing, so it changes freely. A `voted` change's notice is its vote's.
    /// Error cases:
    /// - from `V55`, a changed curve `check_curve_bound` refuses
    /// - from `V55`, unless `voted`, a changed curve no notice names, or whose notice takes effect
    ///   later than the sequenced time
    pub(crate) fn check_curve_change(
        &self,
        curve: &[CurvePoint],
        current: &[CurvePoint],
        voted: bool,
    ) -> Result<(), String> {
        if curve == current || self.protocol_version < ProtocolVersion::V55 {
            return Ok(());
        }
        self.check_curve_bound(curve)?;
        if voted {
            return Ok(());
        }
        let named =
            self.operator_bounds.notice.as_ref().filter(|notice| notice.profit_fee_curve.as_deref() == Some(curve));
        match named {
            None => Err("a profit_fee_curve change needs a notice of at least seven days".into()),
            Some(notice) if notice.effective_at > self.now => {
                Err(format!("the profit_fee_curve change takes effect from {}", notice.effective_at))
            }
            Some(_) => Ok(()),
        }
    }

    /// Starts the holding-time fee when `version` activates `V55` over the log: a base cut below its
    /// launch value earlier rises back to it, parameters that name no curve take the default one, the
    /// curve in force becomes the launch curve, and each held position takes the open time `now - the
    /// curve's last hold`.
    pub(crate) fn start_hold_clocks(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V55 || version < ProtocolVersion::V55 {
            return;
        }
        let now = self.hold_clock_time();
        let Some(params) = self.params.as_mut() else { return };
        // `V35`'s range let the base fall to half its launch value; from `V55` launch is its floor.
        if let Some(launch) = self.operator_bounds.launch {
            params.profit_fee_bps = params.profit_fee_bps.max(launch.profit_fee_bps);
        }
        if params.profit_fee_curve.is_empty() {
            params.profit_fee_curve = DEFAULT_PROFIT_FEE_CURVE.to_vec();
        }
        self.operator_bounds.launch_curve = Some(params.profit_fee_curve.clone());
        let opened_at = now - last_hold(&params.profit_fee_curve);
        self.ensure_hot_indexes();
        let held: Vec<(MarketId, AccountId)> = self
            .hot_indexes
            .as_ref()
            .expect("initialized")
            .holders
            .iter()
            .flat_map(|(market, holders)| holders.iter().map(|account| (*market, *account)))
            .collect();
        for (market, account) in held {
            self.accounts.get_mut(&account).expect("a holder is an account").opened_at.insert(market, opened_at);
            self.record_position(account, market);
        }
    }
}

/// Checks a curve's shape: at most `MAX_CURVE_POINTS` points, holds strictly ascending from zero to
/// at most `MAX_CURVE_HOLD_MS`, and fees from 0 to 10000 bps that never rise with the hold. An empty
/// curve charges the base alone.
/// Error cases:
/// - more than `MAX_CURVE_POINTS` points
/// - a negative hold, holds not strictly ascending, or a last hold past `MAX_CURVE_HOLD_MS`
/// - a fee outside 0..=10000 bps, or one above the fee before it
pub(crate) fn validate_curve(curve: &[CurvePoint]) -> Result<(), String> {
    if curve.len() > MAX_CURVE_POINTS {
        return Err(format!("profit_fee_curve has more than {MAX_CURVE_POINTS} points"));
    }
    if curve.iter().any(|point| point.hold_ms < 0 || !(0..=MAX_RATE_BPS).contains(&point.fee_bps)) {
        return Err("profit_fee_curve needs holds of at least 0 and fees in 0..=10000".into());
    }
    if curve.windows(2).any(|pair| pair[0].hold_ms >= pair[1].hold_ms || pair[0].fee_bps < pair[1].fee_bps) {
        return Err("profit_fee_curve needs ascending holds and fees that never rise".into());
    }
    if curve.last().is_some_and(|point| point.hold_ms > MAX_CURVE_HOLD_MS) {
        return Err(format!("profit_fee_curve reaches at most {MAX_CURVE_HOLD_MS} ms"));
    }
    Ok(())
}

/// Spends `notice`'s curve once parameters move the curve from `before` to the `applied` one it names.
pub(crate) fn spend_curve_notice(notice: &mut ParamsNotice, before: &[CurvePoint], applied: &[CurvePoint]) {
    if applied != before && notice.profit_fee_curve.as_deref() == Some(applied) {
        notice.profit_fee_curve = None;
    }
}

/// The hold at and past which `curve` charges nothing: its last point's, or zero for an empty curve.
pub(crate) fn last_hold(curve: &[CurvePoint]) -> Millis {
    curve.last().map_or(0, |point| point.hold_ms)
}

/// The fee rate on a gain held `hold_ms`: the larger of `base` and `curve`'s fee there.
pub fn fee_at(base: i64, curve: &[CurvePoint], hold_ms: Millis) -> i64 {
    base.max(curve_fee_bps(curve, hold_ms))
}

/// `curve`'s fee at `hold_ms`: the first fee at and below the first hold, linear between points
/// rounded up to the bp, and zero at and past the last hold.
pub fn curve_fee_bps(curve: &[CurvePoint], hold_ms: Millis) -> i64 {
    let past = curve.partition_point(|point| point.hold_ms <= hold_ms);
    match past {
        0 => curve.first().map_or(0, |point| point.fee_bps),
        n if n == curve.len() => 0,
        n => {
            let (from, to) = (curve[n - 1], curve[n]);
            let (span, left) = (i128::from(to.hold_ms - from.hold_ms), i128::from(to.hold_ms - hold_ms));
            // The fee falls by `from.fee - to.fee` over the span; `left` of it is still to fall.
            let above = ceil_div(i128::from(from.fee_bps - to.fee_bps) * left, span);
            to.fee_bps + i64::try_from(above).expect("at most the fall between two points")
        }
    }
}

/// The least hold past `hold_ms` at which `fee_at(base, curve, ..)` is lower than at `hold_ms`, or
/// `None` when it never falls again: once the curve is at or below the base.
pub fn next_fall(base: i64, curve: &[CurvePoint], hold_ms: Millis) -> Option<Millis> {
    let now = curve_fee_bps(curve, hold_ms);
    if now <= base {
        return None;
    }
    // The curve is above the base, so it has points and `hold_ms` is short of the last. It next falls
    // in the first segment from here whose far end is below `now`, where its linear part reaches
    // `now - 1`; before that every segment it crosses is flat at `now`.
    let first = curve.partition_point(|point| point.hold_ms <= hold_ms).max(1);
    let falls = (first..curve.len()).map(|n| (curve[n - 1], curve[n])).find(|(_, to)| to.fee_bps < now);
    Some(match falls {
        Some((from, to)) => {
            let (span, fall) = (i128::from(to.hold_ms - from.hold_ms), i128::from(from.fee_bps - to.fee_bps));
            let along = ceil_div(i128::from(from.fee_bps - now + 1) * span, fall);
            from.hold_ms + i64::try_from(along).expect("within the segment")
        }
        // Flat to the last point, where the curve drops to zero.
        None => curve[curve.len() - 1].hold_ms,
    })
}

/// The first hold at which `curve`, before rounding, is below `launch`, or `None` when it is at or
/// above it at every hold. Both are linear between their points, so comparing them at every point of
/// either up to the launch curve's last hold, past which it is zero, compares them everywhere, once
/// `curve` reaches at least that far.
pub(crate) fn first_hold_below(curve: &[CurvePoint], launch: &[CurvePoint]) -> Option<Millis> {
    let last = launch.last()?.hold_ms;
    match curve.last() {
        Some(point) if point.hold_ms >= last => {}
        point => return Some(point.map_or(0, |point| point.hold_ms)),
    }
    let mut holds: Vec<Millis> = curve.iter().chain(launch).map(|point| point.hold_ms).filter(|h| *h <= last).collect();
    holds.sort_unstable();
    holds.into_iter().find(|hold| {
        let ((num, den), (launch_num, launch_den)) = (extended(curve, *hold), extended(launch, *hold));
        num * launch_den < launch_num * den
    })
}

/// `curve` at `hold_ms` before rounding, continued at its last fee past its last point, as a
/// fraction with a positive denominator.
fn extended(curve: &[CurvePoint], hold_ms: Millis) -> (i128, i128) {
    let past = curve.partition_point(|point| point.hold_ms <= hold_ms);
    if past == 0 || past == curve.len() {
        let point = if past == 0 { curve[0] } else { curve[past - 1] };
        return (i128::from(point.fee_bps), 1);
    }
    let (from, to) = (curve[past - 1], curve[past]);
    let span = i128::from(to.hold_ms - from.hold_ms);
    let fallen = i128::from(from.fee_bps - to.fee_bps) * i128::from(hold_ms - from.hold_ms);
    (i128::from(from.fee_bps) * span - fallen, span)
}

/// The open time after a fill takes a position of `before` units, opened at `opened_at`, to `after`
/// units at `now`, the units held lending at most `last_hold` of age (`Engine::add_clamp_hold`): none
/// once flat; now for an opening or a flip; for an add, the size-weighted average of now and the open
/// time raised to no earlier than `now - last_hold`, rounded towards now; and the open time as it was
/// for a reduction.
pub(crate) fn opened_after(
    before: Fixed,
    opened_at: Option<Millis>,
    after: Fixed,
    now: Millis,
    last_hold: Millis,
) -> Option<Millis> {
    if after.is_zero() {
        return None;
    }
    let Some(opened_at) = opened_at.filter(|_| !before.is_zero() && before.signum() == after.signum()) else {
        return Some(now);
    };
    if after.abs() <= before.abs() {
        return Some(opened_at);
    }
    // A held position lends an add at most `last_hold` of age, so a small aged position cannot carry a
    // large fresh lot past the curve (the owner's decision of 2026-11-13 on #689).
    let opened_at = opened_at.max(now - last_hold);
    // `opened_at + (now - opened_at) * added / after`, with the share of the wait rounded up.
    let elapsed = Fixed::from_raw(i128::from(now - opened_at));
    let moved = elapsed.mul_div(after.abs() - before.abs(), after.abs(), Rounding::Up).raw();
    Some(opened_at + i64::try_from(moved).expect("at most the time since the open"))
}

/// A fill's hold and fee rate, as `Fill` names them: `hold` and `fee_bps` when the fill takes
/// `delta` off a position of `held` units, closing some of it, and neither otherwise.
pub(crate) fn closed_terms(
    held: Fixed,
    delta: Fixed,
    hold: Option<Millis>,
    fee_bps: i64,
) -> (Option<Millis>, Option<i64>) {
    let hold = hold.filter(|_| !held.is_zero() && held.signum() != delta.signum());
    (hold, hold.map(|_| fee_bps))
}

/// `num / den` rounded up, for `num >= 0` and `den > 0`.
fn ceil_div(num: i128, den: i128) -> i128 {
    (num + den - 1) / den
}

/// From `V55`, moves `market`'s entry in `opened_at` as a fill taking its position from `held` to
/// `after` units at `now`, the units held lending at most `last_hold` of age, moves it.
pub(crate) fn move_open_time(
    version: ProtocolVersion,
    opened_at: &mut BTreeMap<MarketId, Millis>,
    market: MarketId,
    (held, after): (Fixed, Fixed),
    (now, last_hold): (Millis, Millis),
) {
    if version < ProtocolVersion::V55 {
        return;
    }
    match opened_after(held, opened_at.get(&market).copied(), after, now, last_hold) {
        Some(at) => opened_at.insert(market, at),
        None => opened_at.remove(&market),
    };
}

#[cfg(test)]
mod tests;
