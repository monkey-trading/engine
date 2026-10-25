//! Trader accounts: cash, netted positions, and resting orders.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::command::OrderKind;
use crate::fixed::Rounding;
use crate::params::NotionalWeighting;
use crate::protocol::ProtocolVersion;
use crate::types::{MarketId, OrderId, Side};

/// One netted position per market, as on a CFD netting account. `qty` is
/// signed: long > 0, short < 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub qty: Fixed,
    pub entry: Fixed,
}

impl Position {
    /// Applies a signed fill and returns the price PnL it realises. From `V5` an averaged entry
    /// rounds against the holder and realised PnL rounds down, so no sequence of fills realises more
    /// than its exact PnL; before `V5` both truncate toward zero.
    pub fn apply(&mut self, delta: Fixed, price: Fixed, version: ProtocolVersion) -> Fixed {
        if delta.is_zero() {
            return Fixed::ZERO;
        }
        let house_rounding = version >= ProtocolVersion::V5;
        if self.qty.is_zero() || self.qty.signum() == delta.signum() {
            let new_qty = self.qty + delta;
            self.entry = if house_rounding {
                // The exact average is `entry + (price - entry) * delta / new_qty`: one rounding, up for
                // a long and down for a short. Weighting the fill keeps the product in range, since a
                // fill adds at most `Fixed::MAX_INPUT`.
                let rounding = if new_qty.is_positive() { Rounding::Up } else { Rounding::Down };
                self.entry + (price - self.entry).mul_div(delta.abs(), new_qty.abs(), rounding)
            } else {
                (self.entry.mul(self.qty.abs()) + price.mul(delta.abs())).div(new_qty.abs())
            };
            self.qty = new_qty;
            return Fixed::ZERO;
        }
        let closing = delta.abs().min(self.qty.abs());
        let pnl = if house_rounding {
            (price - self.entry).mul_int(self.qty.signum()).mul_rounded(closing, Rounding::Down)
        } else {
            (price - self.entry).mul(closing).mul_int(self.qty.signum())
        };
        let new_qty = self.qty + delta;
        if new_qty.is_zero() {
            *self = Position::default();
        } else if new_qty.signum() != self.qty.signum() {
            *self = Position { qty: new_qty, entry: price };
        } else {
            self.qty = new_qty;
        }
        pnl
    }

    /// Price PnL if the position were closed at `mark`: from `V5` rounded down, as closing would
    /// realise it, and before `V5` truncated toward zero.
    pub fn unrealised(&self, mark: Fixed, version: ProtocolVersion) -> Fixed {
        if version >= ProtocolVersion::V5 {
            (mark - self.entry).mul_rounded(self.qty, Rounding::Down)
        } else {
            (mark - self.entry).mul(self.qty)
        }
    }

    /// `entry * qty`, kept per market in aggregate so house NAV is O(markets).
    pub fn cost(&self) -> Fixed {
        self.entry.mul(self.qty)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RestingOrder {
    pub order_id: OrderId,
    pub client_id: u64,
    pub market: MarketId,
    pub side: Side,
    pub qty: Fixed,
    pub kind: OrderKind,
    pub reduce_only: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// Cash: deposits - withdrawals + realised PnL - fees - financing.
    pub balance: Fixed,
    /// Principal credited before Ethereum finality: collateral, excluded from withdrawals.
    #[serde(default, skip_serializing_if = "Fixed::is_zero")]
    pub unfinalised_deposits: Fixed,
    pub positions: BTreeMap<MarketId, Position>,
    pub orders: BTreeMap<OrderId, RestingOrder>,
    /// EWMA of fill markouts in whole bps; see `ToxicityParams`. From `V11` it is the score rounded
    /// down, and `toxicity_fraction` holds the rest.
    pub toxicity_bps: i64,
    /// From `V11`, the part of the score above `toxicity_bps`, in `[0, 1)` bps. Left out of the
    /// encoding while zero.
    #[serde(default, skip_serializing_if = "Fixed::is_zero")]
    pub toxicity_fraction: Fixed,
    /// From `V11`, the fill notional of history behind the score, its prior included, at most the
    /// memory (`NotionalWeighting`). Zero, and left out of the encoding, until a `V11` markout scores
    /// the account.
    #[serde(default, skip_serializing_if = "Fixed::is_zero")]
    pub toxicity_history: Fixed,
    pub withdrawal_nonce: u64,
}

impl Account {
    /// The account's position in `market`, flat if it has none.
    pub fn position(&self, market: MarketId) -> Position {
        self.positions.get(&market).copied().unwrap_or_default()
    }

    /// The toxicity score in bps: whole bps before `V11`, and to nine decimals from it.
    pub fn toxicity_score(&self) -> Fixed {
        Fixed::from_int(self.toxicity_bps) + self.toxicity_fraction
    }

    /// The history behind the score under `weighting`: what `V11` markouts recorded, or else a whole
    /// memory for a score from before `V11`, and the prior for an account with no score yet.
    pub(crate) fn toxicity_history_under(&self, weighting: &NotionalWeighting) -> Fixed {
        if !self.toxicity_history.is_zero() {
            self.toxicity_history
        } else if !self.toxicity_score().is_zero() {
            weighting.memory
        } else {
            weighting.prior
        }
    }

    /// Stores `score` as its whole bps, rounded down, and the fraction of a bp above them.
    pub(crate) fn set_toxicity_score(&mut self, score: Fixed) {
        let whole = score.raw().div_euclid(Fixed::ONE.raw());
        // A score is a weighted average of markouts clamped to ±1000 bps.
        self.toxicity_bps = i64::try_from(whole).expect("a score is within ±1000 bps");
        self.toxicity_fraction = score - Fixed::from_int(self.toxicity_bps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::arbitrary::any;
    use proptest::prop_assert;
    use proptest::prop_assert_eq;
    use proptest::strategy::Strategy;
    use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

    const V2: ProtocolVersion = ProtocolVersion::V2;
    const V5: ProtocolVersion = ProtocolVersion::V5;

    #[test]
    fn netting_averages_entry_and_realises_on_reduce() {
        for version in [V2, V5] {
            let mut p = Position::default();
            assert_eq!(p.apply(f("1"), f("100"), version), Fixed::ZERO);
            assert_eq!(p.apply(f("1"), f("110"), version), Fixed::ZERO);
            assert_eq!(p.entry, f("105"));
            assert_eq!(p.apply(f("-1"), f("120"), version), f("15"));
            assert_eq!(p, Position { qty: f("1"), entry: f("105") });
            assert_eq!(p.unrealised(f("100"), version), f("-5"));
        }
    }

    #[test]
    fn flip_closes_then_opens_at_fill_price() {
        for version in [V2, V5] {
            let mut p = Position { qty: f("2"), entry: f("100") };
            assert_eq!(p.apply(f("-5"), f("90"), version), f("-20"));
            assert_eq!(p, Position { qty: f("-3"), entry: f("90") });
            assert_eq!(p.apply(f("3"), f("80"), version), f("30"));
            assert_eq!(p, Position::default());
        }
    }

    #[test]
    fn a_dust_add_at_the_entry_price_leaves_the_entry_there() {
        let long = Position { qty: f("4500000"), entry: f("1.1") };
        let mut p = long;
        p.apply(f("0.000000001"), f("1.1"), V5);
        assert_eq!(p.entry, f("1.1"));
        // Before `V5` each truncation lowers the long's entry by one unit.
        let mut p = long;
        p.apply(f("0.000000001"), f("1.1"), V2);
        assert_eq!(p.entry, f("1.099999999"));
    }

    #[test]
    fn an_averaged_entry_rounds_up_for_a_long_and_down_for_a_short() {
        // One at 100 and two at 101 average 100.6666…
        let mut long = Position { qty: f("1"), entry: f("100") };
        long.apply(f("2"), f("101"), V5);
        assert_eq!(long.entry, f("100.666666667"));
        let mut short = Position { qty: f("-1"), entry: f("100") };
        short.apply(f("-2"), f("101"), V5);
        assert_eq!(short.entry, f("100.666666666"));
    }

    #[test]
    fn realised_and_unrealised_pnl_round_down() {
        // Half a unit one nano-dollar against the holder is a loss of half a unit of precision.
        let long = Position { qty: f("0.5"), entry: f("100") };
        assert_eq!(long.unrealised(f("99.999999999"), V5), f("-0.000000001"));
        assert_eq!(long.unrealised(f("99.999999999"), V2), Fixed::ZERO);
        assert_eq!(close(long, f("99.999999999"), V5), f("-0.000000001"));
        assert_eq!(close(long, f("99.999999999"), V2), Fixed::ZERO);
        let short = Position { qty: f("-0.5"), entry: f("100") };
        assert_eq!(short.unrealised(f("100.000000001"), V5), f("-0.000000001"));
        assert_eq!(close(short, f("100.000000001"), V5), f("-0.000000001"));
        assert_eq!(close(short, f("100.000000001"), V2), Fixed::ZERO);
        // A gain of half a unit of precision rounds down to nothing.
        assert_eq!(close(long, f("100.000000001"), V5), Fixed::ZERO);
    }

    #[test]
    fn adding_at_a_price_never_raises_what_closing_there_realises() {
        let raw = 1i128..=10i128.pow(15);
        let cases = (raw.clone(), raw.clone(), raw.clone(), raw, any::<bool>());
        check(cases, |(qty, entry, add, price, long)| {
            let sign = if long { 1 } else { -1 };
            let held = Position { qty: Fixed::from_raw(sign * qty), entry: Fixed::from_raw(entry) };
            let price = Fixed::from_raw(price);
            let mut added = held;
            added.apply(Fixed::from_raw(sign * add), price, V5);
            let (with_add, without) = (close(added, price, V5), close(held, price, V5));
            prop_assert!(with_add <= without, "{held:?} + {add} at {price}: {with_add} > {without}");
            Ok(())
        });
    }

    #[test]
    fn unrealised_pnl_is_what_closing_at_the_mark_realises() {
        let raw = 1i128..=10i128.pow(15);
        check((raw.clone(), raw.clone(), raw, any::<bool>()), |(qty, entry, mark, long)| {
            let qty = Fixed::from_raw(if long { qty } else { -qty });
            let held = Position { qty, entry: Fixed::from_raw(entry) };
            let mark = Fixed::from_raw(mark);
            prop_assert_eq!(held.unrealised(mark, V5), close(held, mark, V5));
            Ok(())
        });
    }

    #[test]
    fn a_toxicity_score_splits_into_whole_bps_rounded_down_and_a_fraction() {
        let cases = [("25", 25, "0"), ("4.999999999", 4, "0.999999999"), ("-0.5", -1, "0.5"), ("-3", -3, "0")];
        for (score, whole, fraction) in cases {
            let mut acct = Account::default();
            acct.set_toxicity_score(f(score));
            assert_eq!((acct.toxicity_bps, acct.toxicity_fraction), (whole, f(fraction)), "{score}");
            assert_eq!(acct.toxicity_score(), f(score));
        }
        // A whole score encodes as before `V11`.
        let mut acct = Account::default();
        acct.set_toxicity_score(f("7"));
        assert!(!serde_json::to_string(&acct).unwrap().contains("toxicity_fraction"));
    }

    // ── Helpers ──

    fn f(s: &str) -> Fixed {
        s.parse().unwrap()
    }

    /// The PnL closing `position` at `price` realises.
    fn close(mut position: Position, price: Fixed, version: ProtocolVersion) -> Fixed {
        position.apply(-position.qty, price, version)
    }

    /// Runs `test` over 4,096 cases from a fixed seed, so every run checks the same ones.
    fn check<S: Strategy>(strategy: S, test: impl Fn(S::Value) -> Result<(), TestCaseError>) {
        let config = Config {
            cases: 4_096,
            rng_seed: RngSeed::Fixed(0x5eed_0004),
            failure_persistence: None,
            ..Config::default()
        };
        if let Err(failure) = TestRunner::new(config).run(&strategy, test) {
            panic!("{failure}");
        }
    }
}
