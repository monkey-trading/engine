//! Keys against brute force over every mid within the ceiling's reach.

use super::*;
use proptest::prelude::*;

/// A close's spread at mid `m`: `rate` tenths of a bp of it, rounded down, plus `jump` raw units every
/// `period`, so a long's close steps back by up to `jump - 1` where the spread jumps, clamped to the
/// ceiling as a close is.
fn stepped(m: i128, rate: i128, jump: i128, period: i128, ceiling: i64) -> i128 {
    (m * rate / 100_000 + jump * (m / period)).min(m * i128::from(ceiling) / 100_000)
}

/// The mids of `domain` at which a `kind` level at `level` on a long when `long` fires, for a close whose
/// spread is `up` when that makes it fire sooner and `down` otherwise.
fn firing(
    long: bool,
    kind: LevelKind,
    level: i128,
    up: &impl Fn(i128) -> i128,
    down: &impl Fn(i128) -> i128,
    domain: std::ops::RangeInclusive<i128>,
) -> Vec<i128> {
    domain
        .filter(|m| match (long, kind) {
            (true, LevelKind::StopLoss) => m - up(*m) <= level,
            (true, LevelKind::TakeProfit) => m - down(*m) >= level,
            (false, LevelKind::StopLoss) => m + up(*m) >= level,
            (false, LevelKind::TakeProfit) => m + down(*m) <= level,
        })
        .collect()
}

/// The key brute force gives: the last firing mid for a key reached from below, the first otherwise.
fn expected(long: bool, kind: LevelKind, fires: &[i128]) -> Option<(i128, bool)> {
    let below = long == (kind == LevelKind::StopLoss);
    let mid = if below { fires.last() } else { fires.first() };
    mid.map(|mid| (*mid, below))
}

proptest! {
    /// Every key is the brute-force key over every mid the ceiling leaves within reach: the last mid a
    /// long's stop loss or a short's take profit fires at, and the first a long's take profit or a short's
    /// stop loss fires at, though a long's close steps back where its spread jumps.
    #[test]
    fn a_key_is_where_brute_force_first_or_last_fires(
        rate in 0i128..3_000,
        jump in 1i128..40,
        period in 1i128..2_000,
        ceiling in 4_000i64..20_000,
        level in 20_000i128..60_000,
        long in any::<bool>(),
        stop in any::<bool>(),
    ) {
        let ceiling = ceiling.max(i64::try_from(rate).unwrap() + 4_000);
        let spread = |m: i128| stepped(m, rate, jump, period, ceiling);
        let spreads = Spreads { up: spread, down: spread, ceiling, floor: 1 };
        let kind = if stop { LevelKind::StopLoss } else { LevelKind::TakeProfit };
        // Past twice the level, a ceiling under 20% keeps every close beyond reach of it.
        let fires = firing(long, kind, level, &spread, &spread, 1..=2 * level);
        let key = spreads.key(long, kind, Fixed::from_raw(level)).map(|key| (key.mid.raw(), key.below));
        prop_assert_eq!(key, expected(long, kind, &fires));
    }

    /// With the spread only bounded, as in a market converting at its own index, a key is the brute-force
    /// key of the bound that fires sooner, so it is never short of a mid the spread between them fires at.
    #[test]
    fn a_bounded_spread_keys_at_the_bound_that_fires_sooner(
        rate in 1i128..3_000,
        jump in 1i128..40,
        period in 1i128..2_000,
        level in 20_000i128..60_000,
        long in any::<bool>(),
        stop in any::<bool>(),
    ) {
        let ceiling = 20_000;
        let up = |m: i128| stepped(m, rate, jump, period, ceiling);
        let down = |m: i128| m * rate / 100_000;
        let spreads = Spreads { up, down, ceiling, floor: 1 };
        let kind = if stop { LevelKind::StopLoss } else { LevelKind::TakeProfit };
        let fires = firing(long, kind, level, &up, &down, 1..=2 * level);
        let key = spreads.key(long, kind, Fixed::from_raw(level)).map(|key| (key.mid.raw(), key.below));
        prop_assert_eq!(key, expected(long, kind, &fires));
    }
}

#[test]
fn a_stop_below_the_floor_is_keyed_at_the_ceilings_reach() {
    let ceiling = 1_000;
    let spread = |m: i128| m * 50 / 100_000;
    let spreads = Spreads { up: spread, down: spread, ceiling, floor: 1_000_000 };
    let long = spreads.key(true, LevelKind::StopLoss, Fixed::from_raw(500_000)).expect("a long stop fires");
    let short = spreads.key(false, LevelKind::StopLoss, Fixed::from_raw(500_000)).expect("a short stop fires");
    // A ceiling of 1% reaches about 1% past the stop, beyond where the spread itself fires.
    assert!(long.below && long.mid.raw() > 505_000, "{long:?}");
    assert!(!short.below && short.mid.raw() < 495_100, "{short:?}");
}

#[test]
fn a_level_no_valid_mid_reaches_has_no_key() {
    let spread = |m: i128| m / 100;
    let spreads = Spreads { up: spread, down: spread, ceiling: 2_000, floor: 1 };
    // A long's take profit past the highest mid, and a short's take profit under the lowest, never fire.
    assert_eq!(spreads.key(true, LevelKind::TakeProfit, Fixed::MAX_INPUT), None);
    assert_eq!(spreads.key(false, LevelKind::TakeProfit, Fixed::from_raw(0)), None);
    // A short's stop loss at the lowest price fires at the lowest mid.
    let stop = spreads.key(false, LevelKind::StopLoss, Fixed::from_raw(1)).expect("fires");
    assert_eq!((stop.mid.raw(), stop.below), (1, false));
}
