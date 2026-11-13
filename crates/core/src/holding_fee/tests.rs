use super::*;
use crate::account::{Account, Position};
use proptest::collection::vec;
use proptest::prop_assert;
use proptest::prop_assert_eq;
use proptest::strategy::Strategy;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

#[test]
fn the_default_curve_holds_the_full_fee_to_ten_seconds_and_reaches_the_base_at_fifteen_minutes() {
    let curve = DEFAULT_PROFIT_FEE_CURVE;
    assert_eq!(validate_curve(&curve), Ok(()));
    // (hold, fee): the full fee to 10 s, the curve between its points rounded up to the bp, and the
    // base from 15 minutes, where the curve is zero.
    let cases = [
        (0, 10_000),
        (10_000, 10_000),
        (10_006, 10_000),
        (10_007, 9_999),
        (15_683, 9_100),
        (60_000, 6_421),
        (600_000, 1_828),
        (899_999, 1_001),
        (900_000, 1_000),
        (1_800_000, 1_000),
    ];
    for (hold, fee) in cases {
        assert_eq!(fee_at(1_000, &curve, hold), fee, "{hold}");
    }
    assert_eq!(curve_fee_bps(&curve, 900_000), 0);
}

#[test]
fn the_default_curve_is_never_below_the_log_linear_curve_and_at_most_052_points_above_it() {
    // The log-linear curve from 10000 bps at 10 s to 1000 at 15 minutes, against the table at every
    // millisecond between. Floats only check the integer table here; nothing in the engine uses them.
    let (mut most, mut least) = (f64::MIN, f64::MAX);
    for hold in 10_000..900_000 {
        let curve = 10_000.0 - 9_000.0 * (hold as f64 / 10_000.0).ln() / 90f64.ln();
        let above = curve_fee_bps(&DEFAULT_PROFIT_FEE_CURVE, hold) as f64 - curve;
        (most, least) = (most.max(above), least.min(above));
    }
    assert!(least >= -1e-6, "below by {least} bps");
    assert!(most <= 52.0, "above by {most} bps");
}

#[test]
fn a_curve_with_one_point_charges_its_fee_up_to_its_hold_and_an_empty_one_nothing() {
    let one = [CurvePoint { hold_ms: 5_000, fee_bps: 3_000 }];
    assert_eq!((curve_fee_bps(&one, 4_999), curve_fee_bps(&one, 5_000)), (3_000, 0));
    assert_eq!(next_fall(1_000, &one, 0), Some(5_000));
    assert_eq!((fee_at(1_000, &[], 0), next_fall(1_000, &[], 0)), (1_000, None));
}

#[test]
fn a_curve_out_of_shape_is_refused() {
    let point = |hold_ms, fee_bps| CurvePoint { hold_ms, fee_bps };
    let cases: [(Vec<CurvePoint>, &str); 6] = [
        (vec![point(-1, 100)], "holds of at least 0"),
        (vec![point(1, 10_001)], "fees in 0..=10000"),
        (vec![point(2, 100), point(2, 50)], "ascending holds"),
        (vec![point(1, 100), point(2, 200)], "never rise"),
        (vec![point(MAX_CURVE_HOLD_MS + 1, 100)], "at most 1800000 ms"),
        ((0..=MAX_CURVE_POINTS as i64).map(|n| point(n, 0)).collect(), "more than 32 points"),
    ];
    for (curve, reason) in cases {
        let error = validate_curve(&curve).unwrap_err();
        assert!(error.contains(reason), "{error}");
    }
    assert_eq!(validate_curve(&[point(0, 10_000), point(MAX_CURVE_HOLD_MS, 0)]), Ok(()));
}

#[test]
fn an_open_time_moves_as_the_position_does() {
    let (now, opened, last_hold) = (100_000, 40_000, 900_000);
    let units = Fixed::from_int;
    // (held, after, open time, open time after): opened, added to long and short, reduced, flipped,
    // and closed.
    let cases = [
        (0, 3, None, Some(now)),
        (1, 3, Some(opened), Some(80_000)),
        (-1, -4, Some(opened), Some(85_000)),
        (3, 1, Some(opened), Some(opened)),
        (3, -1, Some(opened), Some(now)),
        (3, 0, Some(opened), None),
    ];
    for (held, after, open, expected) in cases {
        let moved = opened_after(units(held), open, units(after), now, last_hold);
        assert_eq!(moved, expected, "{held} -> {after}");
    }
    // An add's share of the wait rounds towards now: a third of a millisecond is a whole one.
    assert_eq!(opened_after(units(2), Some(0), units(3), 1, last_hold), Some(1));
    // An add counts the open time from no earlier than the curve's last hold before now: 60 s held
    // under a 30 s curve lends the add 30 s, two thirds of which it averages away. A reduction keeps
    // the open time, and an empty curve, which charges nothing past a hold of zero, lends none.
    assert_eq!(opened_after(units(1), Some(opened), units(3), now, 30_000), Some(90_000));
    assert_eq!(opened_after(units(3), Some(opened), units(1), now, 30_000), Some(opened));
    assert_eq!(opened_after(units(1), Some(opened), units(3), now, 0), Some(now));
}

#[test]
fn an_add_moves_the_open_time_to_the_size_weighted_average_of_now_and_the_open_time_within_the_curve() {
    let raw = 1i128..=10i128.pow(18);
    let times = (0i64..=1_000_000_000, 0i64..=1_000_000_000, 0..=MAX_CURVE_HOLD_MS);
    check((raw.clone(), raw, times), |(held, added, (opened, wait, last_hold))| {
        let (before, after, now) = (Fixed::from_raw(held), Fixed::from_raw(held + added), opened + wait);
        let at = opened_after(before, Some(opened), after, now, last_hold).unwrap();
        // The units held count from no earlier than the curve's last hold before now, so the add's
        // never look older than the curve's end.
        let from = opened.max(now - last_hold);
        prop_assert!(from <= at && at <= now, "{at}");
        // The exact size-weighted average, scaled by the size after, is at or below `at`'s and less
        // than a millisecond's worth below it.
        let exact = i128::from(from) * (held + added) + i128::from(now - from) * added;
        let rounded = i128::from(at) * (held + added);
        prop_assert!(rounded >= exact && rounded - exact < held + added, "{at}");
        Ok(())
    });
}

#[test]
fn the_fee_never_rises_with_the_hold_and_next_fall_names_where_it_drops() {
    check((curves(), 0i64..=1_500), |(curve, base)| {
        let end = curve.last().map_or(0, |point| point.hold_ms) + 2;
        let fees: Vec<i64> = (0..=end).map(|hold| fee_at(base, &curve, hold)).collect();
        // Walking back from the end, the next fall from each hold is the first hold after it at which
        // the fee changed.
        let mut fall = None;
        for hold in (0..=end).rev() {
            prop_assert_eq!(next_fall(base, &curve, hold), fall, "{}", hold);
            let at = hold as usize;
            if at > 0 {
                prop_assert!(fees[at] <= fees[at - 1], "{hold}: {} > {}", fees[at], fees[at - 1]);
                if fees[at] < fees[at - 1] {
                    fall = Some(hold);
                }
            }
        }
        Ok(())
    });
}

#[test]
fn a_curve_first_hold_below_admits_is_at_or_above_its_launch_at_every_hold() {
    check((curves(), curves()), |(curve, launch)| {
        let reach = curve.iter().chain(&launch).map(|point| point.hold_ms).max().unwrap_or(0) + 2;
        let below = (0..=reach).find(|hold| curve_fee_bps(&curve, *hold) < curve_fee_bps(&launch, *hold));
        match first_hold_below(&curve, &launch) {
            // Admitted: rounded up from a curve at or above launch's, every fee is at least launch's.
            None => prop_assert_eq!(below, None),
            // Refused: the curve ends before launch's, or before rounding it is below at the hold named.
            Some(hold) => {
                let reaches = curve.last().is_some_and(|point| point.hold_ms >= launch[launch.len() - 1].hold_ms);
                if reaches {
                    let ((num, den), (launch_num, launch_den)) = (extended(&curve, hold), extended(&launch, hold));
                    prop_assert!(num * launch_den < launch_num * den, "{}", hold);
                } else {
                    prop_assert_eq!(hold, curve.last().map_or(0, |point| point.hold_ms));
                }
            }
        }
        Ok(())
    });
}

#[test]
fn a_raised_or_longer_curve_is_admitted_and_a_lowered_or_shorter_one_refused() {
    let launch = DEFAULT_PROFIT_FEE_CURVE;
    let raised: Vec<CurvePoint> =
        launch.iter().map(|point| CurvePoint { fee_bps: (point.fee_bps + 500).min(10_000), ..*point }).collect();
    let mut longer = launch.to_vec();
    longer.push(CurvePoint { hold_ms: MAX_CURVE_HOLD_MS, fee_bps: 1_000 });
    assert_eq!((first_hold_below(&raised, &launch), first_hold_below(&longer, &launch)), (None, None));
    let mut lowered = launch.to_vec();
    lowered[4].fee_bps -= 1;
    assert_eq!(first_hold_below(&lowered, &launch), Some(60_492));
    assert_eq!(first_hold_below(&launch[..10], &launch), Some(573_877));
    assert_eq!(first_hold_below(&[], &launch), Some(0));
    assert_eq!(first_hold_below(&[], &[]), None);
}

#[test]
fn a_fill_names_a_hold_only_when_it_closes_some_of_the_position() {
    let units = Fixed::from_int;
    // (held, delta, the hold and rate it names): an opening, an add, a reduction and a flip.
    let cases = [(0, 1, None), (1, 1, None), (2, -1, Some((5, 10_000))), (-1, 3, Some((5, 10_000)))];
    for (held, delta, named) in cases {
        let terms = closed_terms(units(held), units(delta), Some(5), 10_000);
        assert_eq!(terms, (named.map(|(hold, _)| hold), named.map(|(_, rate)| rate)), "{held} {delta}");
    }
    assert_eq!(closed_terms(units(2), units(-1), None, 1_000), (None, None), "before V55");
}

#[test]
fn a_hold_runs_to_the_commands_stamp_while_a_catch_up_has_rewound_the_clock() {
    // An open time activation set 15 minutes before its command, and a close-out at a rollover the
    // next command's catch-up reaches 30 minutes before that command: measured to the command, the
    // hold is 15 minutes and pays the base, where one measured to the boundary would pay the full fee.
    let command = 1_800_000_000_000;
    let mut engine = holding(command - 900_000);
    (engine.now, engine.command_ms) = (command - 1_800_000, command);
    assert_eq!(engine.close_fee_terms(HOLDER, 1), (Some(900_000), 1_000));
    // Outside a command, as on an engine restored before it applies one, the last sequenced time.
    (engine.now, engine.command_ms) = (command, 0);
    assert_eq!(engine.close_fee_terms(HOLDER, 1), (Some(900_000), 1_000));
    let fee = engine.close_fee(HOLDER, 1).unwrap();
    assert_eq!((fee.opened_at, fee.fee_bps, fee.falls_at), (command - 900_000, 1_000, None));
}

// ── Helpers ──

/// Curves of up to six points over the first two seconds, so every millisecond can be walked.
fn curves() -> impl Strategy<Value = Vec<CurvePoint>> {
    vec((1i64..=300, 0i64..=2_000), 0..=6).prop_map(|steps| {
        let (mut hold, mut fee) = (0, 10_000);
        steps
            .into_iter()
            .map(|(gap, fall)| {
                hold += gap;
                fee = (fee - fall).max(0);
                CurvePoint { hold_ms: hold, fee_bps: fee }
            })
            .collect()
    })
}

const HOLDER: AccountId = AccountId([1; 20]);

/// A `V55` engine whose one account holds a unit of market 1, opened at `opened_at`, under the
/// default curve and a 10% base.
fn holding(opened_at: Millis) -> Engine {
    let mut engine = Engine::new();
    engine.protocol_version = ProtocolVersion::V55;
    let params = serde_json::json!({
        "profit_fee_bps": 1_000,
        "closeout_level_bps": 5_000,
        "house_total_cap_bps": 20_000,
        "stress_levels": [{ "size_spread_multiplier_bps": 10_000, "min_spread_bps": 0 }],
        "stress_move_window_ms": 60_000,
        "stress_cooldown_ms": 0,
        "toxicity": { "horizon_ms": 1_000, "ewma_weight_bps": 5_000, "tiers": [] },
        "rollover": { "tz": "Europe/London", "hour": 22, "minute": 0 },
        "profit_fee_curve": DEFAULT_PROFIT_FEE_CURVE,
    });
    engine.params = Some(serde_json::from_value(params).unwrap());
    let mut acct = Account::default();
    acct.positions.insert(1, Position { qty: Fixed::ONE, entry: Fixed::ONE });
    acct.opened_at.insert(1, opened_at);
    engine.accounts.insert(HOLDER, acct);
    engine
}

/// Runs `test` over 1,024 cases from a fixed seed, so every run checks the same ones.
fn check<S: Strategy>(strategy: S, test: impl Fn(S::Value) -> Result<(), TestCaseError>) {
    let config =
        Config { cases: 1_024, rng_seed: RngSeed::Fixed(0x5eed_0053), failure_persistence: None, ..Config::default() };
    if let Err(failure) = TestRunner::new(config).run(&strategy, test) {
        panic!("{failure}");
    }
}
