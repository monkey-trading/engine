use super::*;

#[test]
fn the_launch_table_validates_and_each_shape_rule_refuses_its_case() {
    assert_eq!(validate_volume_tiers(&launch_volume_tiers()), Ok(()));
    assert_eq!(validate_volume_tiers(&[]), Ok(()));
    let cases: [(&str, Change); 8] = [
        ("tiers", |t| t.extend((0..5).map(|n| tier(300_000_000 + n, 10, 25_000)))),
        ("tier 0", |t| t[0].threshold = usd(1)),
        ("tier 0", |t| t[0].day_divisor = 2),
        ("ascending", |t| t[2].threshold = t[1].threshold),
        ("ascending", |t| t[3].threshold = Fixed::MAX_INPUT + Fixed::from_raw(1)),
        ("day_divisor", |t| t[1].day_divisor = 31),
        ("best_price_share_bps", |t| t[3].best_price_share_bps = 100_001),
        ("best_price_share_bps", |t| t[2].best_price_share_bps = 4_999),
    ];
    for (named, change) in cases {
        let mut tiers = launch_volume_tiers();
        change(&mut tiers);
        let refused = validate_volume_tiers(&tiers).unwrap_err();
        assert!(refused.contains(named), "{named}: {refused}");
    }
}

#[test]
fn a_day_counts_at_most_its_cap_so_a_tier_takes_its_days() {
    let tiers = launch_volume_tiers();
    // (counted volume by day, newest last, tier): $1M on one day reaches only half of tier 1, which
    // two days of $500k reach; $25M takes five days of $5M.
    let cases: [(&[i64], usize); 6] = [
        (&[], 0),
        (&[1_000_000], 0),
        (&[500_000, 499_999], 0),
        (&[500_000, 500_000], 1),
        (&[25_000_000, 5_000_000, 5_000_000, 5_000_000], 1),
        (&[5_000_000, 5_000_000, 5_000_000, 5_000_000, 5_000_000], 2),
    ];
    for (days, expected) in cases {
        let mut window = [Fixed::ZERO; WINDOW_DAYS];
        for (slot, volume) in window.iter_mut().rev().zip(days.iter().rev()) {
            *slot = usd(*volume);
        }
        assert_eq!(tier_of(&window, &tiers), expected, "{days:?}");
    }
}

#[test]
fn a_cap_the_divisor_does_not_divide_still_reaches_its_threshold_over_its_days() {
    // $1M over 30 days caps each day at a third of a cent short of $33,333.34: thirty days just over
    // the cap reach the tier, and thirty a unit under it do not.
    let tiers = [tier(0, 1, 2_500), tier(1_000_000, 30, 5_000)];
    let over = [usd_raw(33_333_333_333_334); WINDOW_DAYS];
    let under = [usd_raw(33_333_333_333_333); WINDOW_DAYS];
    assert_eq!((tier_of(&over, &tiers), tier_of(&under, &tiers)), (1, 0));
    assert_eq!(capped_times_divisor(&over, &tiers[1]), usd(1_000_000).raw() * 30);
}

#[test]
fn a_tiers_share_moves_only_the_first_bound_and_never_past_the_second() {
    let tiers = bands();
    let bound = |share| AccountBands::new(&tiers, share).best_price_bound();
    assert_eq!(bound(2_500), Some(usd(500_000)));
    assert_eq!(bound(5_000), Some(usd(1_000_000)));
    assert_eq!(bound(10_000), Some(usd(2_000_000)));
    assert_eq!(bound(25_000), Some(usd(5_000_000)));
    assert_eq!(bound(60_000), Some(usd(10_000_000)));
    // A $600k fill from flat at a quarter pays 2bp to $500k and 4bp on the $100k above; no rate moves.
    let quarter = AccountBands::new(&tiers, 2_500);
    assert_eq!(quarter.staircase(Fixed::ZERO, usd(600_000), Rounding::Up), usd(140));
    assert_eq!(AccountBands::new(&tiers, 10_000).staircase(Fixed::ZERO, usd(600_000), Rounding::Up), usd(120));
    let rates: Vec<i64> = quarter.tiers().iter().map(SpreadTier::tenth_bps).collect();
    assert_eq!(rates, [20, 40, 80]);
    // Past the second bound, the second band is empty and the bounds name it once.
    let wide = AccountBands::new(&tiers, 60_000);
    assert_eq!(wide.bounds(), [usd(10_000_000)]);
    assert_eq!(wide.staircase(usd(9_000_000), usd(11_000_000), Rounding::Up), usd(200 + 800));
    // The second band reaches down to the first's new end.
    assert_eq!(quarter.bounds(), [usd(500_000), usd(10_000_000)]);
}

#[test]
fn a_table_of_one_band_is_as_listed_and_no_share_takes_a_bound_past_the_input_bound_or_itself() {
    let one = [SpreadTier::tenths(None, 20)];
    let bands = AccountBands::new(&one, 2_500);
    assert_eq!((bands.best_price_bound(), bands.bounds()), (None, vec![]));
    let two = |bound: Fixed| [SpreadTier::tenths(Some(bound), 20), SpreadTier::tenths(None, 40)];
    // Ten times a fifth of the input bound stops at the input bound, and a bound past it, even one too
    // large to scale, stays where it is listed.
    let fifth = two(Fixed::MAX_INPUT.div_int(5));
    assert_eq!(AccountBands::new(&fifth, 100_000).best_price_bound(), Some(Fixed::MAX_INPUT));
    let huge = two(Fixed::from_raw(i128::MAX / 2));
    assert_eq!(AccountBands::new(&huge, 100_000).best_price_bound(), Some(Fixed::from_raw(i128::MAX / 2)));
    assert_eq!(AccountBands::new(&huge, 2_500).best_price_bound(), Some(Fixed::from_raw(i128::MAX / 8)));
}

#[test]
fn days_are_utc_days_of_sequenced_time() {
    assert_eq!((day_of(0), day_of(DAY_MS - 1), day_of(DAY_MS), day_of(-1)), (0, 0, 1, -1));
}

// ── Helpers ──

/// An edit of a tier table.
type Change = fn(&mut Vec<VolumeTier>);

fn usd(n: i64) -> Fixed {
    Fixed::from_int(n)
}

fn usd_raw(raw: i128) -> Fixed {
    Fixed::from_raw(raw)
}

fn tier(threshold: i64, day_divisor: i64, best_price_share_bps: i64) -> VolumeTier {
    VolumeTier { threshold: usd(threshold), day_divisor, best_price_share_bps }
}

/// BTC-USD's bands in the dev genesis: 2bp to $2M, 4bp to $10M and 8bp above.
fn bands() -> Vec<SpreadTier> {
    vec![
        SpreadTier::tenths(Some(usd(2_000_000)), 20),
        SpreadTier::tenths(Some(usd(10_000_000)), 40),
        SpreadTier::tenths(None, 80),
    ]
}
