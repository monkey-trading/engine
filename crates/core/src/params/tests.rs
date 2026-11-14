use super::*;
use chrono::TimeZone;

fn rollover(tz: Tz, hour: u32, minute: u32) -> Rollover {
    Rollover { tz, hour, minute }
}

fn f(s: &str) -> Fixed {
    s.parse().unwrap()
}

fn utc_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Millis {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
}

fn params() -> ProtocolParams {
    ProtocolParams {
        profit_fee_bps: 1_000,
        closeout_level_bps: 5_000,
        house_total_cap_bps: 20_000,
        stress_levels: vec![StressLevel { size_spread_multiplier_bps: 10_000, min_spread_bps: 0, trigger: None }],
        stress_move_window_ms: 60_000,
        stress_cooldown_ms: 0,
        toxicity: ToxicityParams {
            horizon_ms: 1_000,
            ewma_weight_bps: 5_000,
            notional: Some(NotionalWeighting { prior: Fixed::from_int(10_000), memory: Fixed::from_int(10_000_000) }),
            tiers: vec![],
            score_tiers: vec![],
        },
        rollover: rollover(chrono_tz::Europe::London, 22, 0),
        house_total_cap_floor: Fixed::ZERO,
        max_resting_orders: DEFAULT_MAX_RESTING_ORDERS,
        profit_fee_curve: vec![],
        volume_tiers: vec![],
    }
}

#[test]
fn rollover_is_strictly_after_and_follows_local_time() {
    let r = rollover(chrono_tz::Europe::London, 22, 0);
    // BST: 22:00 London is 21:00 UTC; a timestamp exactly at the rollover moves to the next day.
    assert_eq!(r.next_after(utc_ms(2026, 10, 20, 12, 0)), utc_ms(2026, 10, 20, 21, 0));
    assert_eq!(r.next_after(utc_ms(2026, 10, 20, 21, 0)), utc_ms(2026, 10, 21, 21, 0));
    // After the clocks go back on 25 Oct, 22:00 London is 22:00 UTC.
    assert_eq!(r.next_after(utc_ms(2026, 10, 26, 12, 0)), utc_ms(2026, 10, 26, 22, 0));
}

#[test]
fn rollover_in_a_dst_gap_moves_an_hour_later() {
    // 01:30 on 29 Mar 2026 does not exist in London; the clocks jump 01:00 -> 02:00 (01:00 UTC).
    let r = rollover(chrono_tz::Europe::London, 1, 30);
    assert_eq!(r.next_after(utc_ms(2026, 3, 28, 23, 0)), utc_ms(2026, 3, 29, 1, 30));
}

#[test]
fn validation_rejects_out_of_range_values() {
    assert!(params().validate().is_ok());
    let cases: [fn(&mut ProtocolParams); 6] = [
        |p| p.profit_fee_bps = 10_000,
        |p| p.closeout_level_bps = 0,
        |p| p.stress_levels.clear(),
        |p| p.stress_levels[0].trigger = Some(StressTrigger::default()),
        |p| p.stress_levels[0].min_spread_bps = i64::MAX,
        |p| p.toxicity.ewma_weight_bps = 0,
    ];
    for mutate in cases {
        let mut p = params();
        mutate(&mut p);
        assert!(p.validate().is_err(), "{p:?}");
    }
}

#[test]
fn markout_horizon_is_positive_and_at_most_an_hour() {
    let mut p = params();
    // The bounds: 1 ms and one hour.
    for horizon_ms in [1, 3_600_000] {
        p.toxicity.horizon_ms = horizon_ms;
        assert!(p.validate().is_ok(), "{horizon_ms}");
    }
    // A markout needs a positive horizon. Past the cap one could hold back every later markout
    // for longer, and near `i64::MAX` its due time would overflow.
    for horizon_ms in [i64::MIN, -1, 0, 3_600_001, i64::MAX] {
        p.toxicity.horizon_ms = horizon_ms;
        assert!(p.validate().is_err(), "{horizon_ms}");
    }
}

#[test]
fn total_cap_floor_is_from_zero_to_the_input_bound() {
    let mut p = params();
    for floor in [Fixed::ZERO, Fixed::from_raw(1), Fixed::MAX_INPUT] {
        p.house_total_cap_floor = floor;
        assert!(p.validate().is_ok(), "{floor}");
    }
    for floor in [Fixed::from_raw(-1), Fixed::MAX_INPUT + Fixed::from_raw(1)] {
        p.house_total_cap_floor = floor;
        assert!(p.validate().is_err(), "{floor}");
    }
}

#[test]
fn the_resting_order_cap_is_at_least_one_and_at_most_a_thousand() {
    let mut p = params();
    for cap in [1, 1_000] {
        p.max_resting_orders = cap;
        assert!(p.validate().is_ok(), "{cap}");
    }
    for cap in [0, 1_001, usize::MAX] {
        p.max_resting_orders = cap;
        assert!(p.validate().is_err(), "{cap}");
    }
}

#[test]
fn a_zero_total_cap_floor_is_left_out_of_the_encoding() {
    let mut p = params();
    assert!(!serde_json::to_string(&p).unwrap().contains("house_total_cap_floor"));
    p.house_total_cap_floor = Fixed::from_int(5);
    let json = serde_json::to_string(&p).unwrap();
    assert!(json.contains(r#""house_total_cap_floor":"5""#), "{json}");
    assert_eq!(serde_json::from_str::<ProtocolParams>(&json).unwrap(), p);
}

#[test]
fn notional_weighting_needs_a_positive_prior_within_its_memory() {
    let weighting = |prior: Fixed, memory: Fixed| NotionalWeighting { prior, memory };
    let one = Fixed::from_raw(1);
    for (prior, memory) in [(one, one), (one, Fixed::MAX_INPUT), (Fixed::MAX_INPUT, Fixed::MAX_INPUT)] {
        assert!(weighting(prior, memory).validate().is_ok(), "{prior} {memory}");
    }
    for (prior, memory) in [(Fixed::ZERO, one), (-one, one), (f("2"), f("1")), (one, Fixed::MAX_INPUT + one)] {
        assert!(weighting(prior, memory).validate().is_err(), "{prior} {memory}");
    }
    let mut p = params();
    p.toxicity.notional = Some(weighting(Fixed::ZERO, one));
    assert!(p.validate().is_err());
    // Parameters set before `V11` have none: they validate as they did, and `V11` refuses them.
    p.toxicity.notional = None;
    assert!(p.validate().is_ok());
    assert!(p.check_notional_weighting().is_err());
    assert!(!serde_json::to_string(&p).unwrap().contains(r#""notional""#));
}

#[test]
fn a_markout_averages_into_the_score_by_its_fills_notional() {
    let weighting = NotionalWeighting { prior: f("10000"), memory: f("100000") };
    let (score, history) = (f("10"), f("10000"));
    // A markout of 30 against 10,000 of history at 10: the notional-weighted average, short of
    // the markout however large the fill, with the history growing up to the memory.
    let cases = [
        ("0.0001", "10.0000002", "10000.0001"),
        ("10000", "20", "20000"),
        ("30000", "25", "40000"),
        ("990000", "29.8", "100000"),
    ];
    for (notional, moved, after) in cases {
        assert_eq!(weighting.averaged(score, history, f("30"), f(notional)), (f(moved), f(after)), "{notional}");
    }
    // At the memory, a fill as large as it moves the score half way, and older history fades.
    let memory = weighting.memory;
    assert_eq!(weighting.averaged(score, memory, f("30"), memory), (f("20"), memory));
    // A whole input bound of qty at a whole bound of price stays in range, and counts in full:
    // rounded up, it takes the score onto a markout above it, and a unit short of one below.
    let huge = Fixed::MAX_INPUT.mul(Fixed::MAX_INPUT);
    assert_eq!(weighting.averaged(score, memory, f("30"), huge), (f("30"), memory));
    assert_eq!(weighting.averaged(score, memory, f("-30"), huge).0, f("-29.999999999"));
    // A step of under one unit of precision rounds up: it raises the score by a unit, here onto
    // the markout, and lowers it not at all.
    assert_eq!(weighting.averaged(score, history, f("10.000000001"), f("1")).0, f("10.000000001"));
    assert_eq!(weighting.averaged(score, history, f("9.999999999"), f("1")).0, score);
}

#[test]
fn parameters_that_name_no_resting_order_cap_keep_their_encoding() {
    // Parameters as a log written before the cap existed records them.
    let mut json = serde_json::to_value(params()).unwrap();
    assert!(json.get("max_resting_orders").is_none(), "{json}");
    let parsed: ProtocolParams = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(parsed.max_resting_orders, DEFAULT_MAX_RESTING_ORDERS);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
    // A cap other than the default is recorded.
    json["max_resting_orders"] = 5.into();
    let parsed: ProtocolParams = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(parsed.max_resting_orders, 5);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
}

#[test]
fn toxicity_spread_takes_the_highest_tier_reached() {
    let mut p = params();
    p.toxicity.tiers = vec![
        ToxicityTier { min_score_bps: 5, extra_spread_bps: 1 },
        ToxicityTier { min_score_bps: 15, extra_spread_bps: 3 },
    ];
    assert_eq!(p.toxicity_spread_bps(4), 0);
    assert_eq!(p.toxicity_spread_bps(5), 1);
    assert_eq!(p.toxicity_spread_bps(20), 3);
}

#[test]
fn from_v52_no_stress_level_multiplies_size_spreads_below_one() {
    let mut p = params();
    p.stress_levels.push(StressLevel {
        size_spread_multiplier_bps: 9_999,
        min_spread_bps: 1,
        trigger: Some(StressTrigger { move_bps: Some(100), ..StressTrigger::default() }),
    });
    // Earlier versions admit any multiplier from zero.
    assert!(p.validate_for(ProtocolVersion::V51).is_ok());
    let err = p.validate_for(ProtocolVersion::V52).unwrap_err();
    assert_eq!(err, "from protocol V52 every stress level's size multiplier is at least 10000 bps");
    p.stress_levels[1].size_spread_multiplier_bps = 10_000;
    assert!(p.validate_for(ProtocolVersion::V52).is_ok());
    p.stress_levels[0].size_spread_multiplier_bps = 0;
    assert!(p.validate_for(ProtocolVersion::V52).is_err());
}
