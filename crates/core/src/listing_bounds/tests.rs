use super::*;

fn f(n: i64) -> Fixed {
    Fixed::from_int(n)
}

/// 0.4bp to 2m, 0.8bp to 10m, 1.6bp above.
fn launch_spreads() -> Vec<SpreadTier> {
    vec![
        SpreadTier::tenths(Some(f(2_000_000)), 4),
        SpreadTier::tenths(Some(f(10_000_000)), 8),
        SpreadTier::tenths(None, 16),
    ]
}

/// 1% to 2m, 2% to 10m, 5% above.
fn launch_margins() -> Vec<Tier> {
    vec![
        Tier { up_to: Some(f(2_000_000)), bps: 100 },
        Tier { up_to: Some(f(10_000_000)), bps: 200 },
        Tier { up_to: None, bps: 500 },
    ]
}

fn spreads(bands: &[(Option<i64>, i64)]) -> Vec<SpreadTier> {
    bands.iter().map(|&(up_to, rate)| SpreadTier::tenths(up_to.map(f), rate)).collect()
}

fn margins(bands: &[(Option<i64>, i64)]) -> Vec<Tier> {
    bands.iter().map(|&(up_to, bps)| Tier { up_to: up_to.map(f), bps }).collect()
}

#[test]
fn a_notice_naming_only_a_markup_encodes_as_v49s_did() {
    let notice = ListingNotice::markup(-150, 7);
    assert_eq!(serde_json::to_string(&notice).unwrap(), r#"{"markup_bps":-150,"effective_at":7}"#);
    let read: ListingNotice = serde_json::from_str(r#"{"markup_bps":-150,"effective_at":7}"#).unwrap();
    assert_eq!(read, notice);
    assert!(!notice.names_nothing());
    assert!(ListingNotice { markup_bps: None, ..notice }.names_nothing());
}

#[test]
fn spread_rates_stay_from_the_launch_rate_to_twice_it_at_every_notional() {
    let launch = launch_spreads();
    // (bands, admitted)
    let cases = [
        (launch.clone(), true),
        (spreads(&[(Some(2_000_000), 8), (Some(10_000_000), 16), (None, 32)]), true),
        (spreads(&[(Some(2_000_000), 9), (Some(10_000_000), 16), (None, 32)]), false),
        (spreads(&[(Some(2_000_000), 3), (Some(10_000_000), 8), (None, 16)]), false),
        (spreads(&[(Some(2_000_000), 4), (Some(10_000_000), 8), (None, 33)]), false),
        // A bound moved down charges the outer rate sooner: 0.8bp from 1m is within twice 0.4bp.
        (spreads(&[(Some(1_000_000), 4), (Some(10_000_000), 8), (None, 16)]), true),
        // A bound moved up charges the inner rate later: 0.4bp from 2m to 3m is below 0.8bp.
        (spreads(&[(Some(3_000_000), 4), (Some(10_000_000), 8), (None, 16)]), false),
        // One band for everything must sit inside every launch band's range at once.
        (spreads(&[(None, 8)]), false),
        (spreads(&[(Some(2_000_000), 8), (None, 16)]), true),
    ];
    for (bands, admitted) in cases {
        let checked = check_spread_within_launch(&bands, &launch);
        assert_eq!(checked.is_ok(), admitted, "{bands:?}: {checked:?}");
    }
}

#[test]
fn the_margin_requirement_stays_at_or_above_launch_at_every_notional() {
    let launch = launch_margins();
    // (bands, admitted)
    let cases = [
        (launch.clone(), true),
        (margins(&[(Some(2_000_000), 200), (Some(10_000_000), 200), (None, 500)]), true),
        // A first band raised and a second lowered: 150k at 10m against launch's 180k.
        (margins(&[(Some(2_000_000), 150), (Some(10_000_000), 150), (None, 500)]), false),
        // Lowered only past the last bound.
        (margins(&[(Some(2_000_000), 100), (Some(10_000_000), 200), (None, 400)]), false),
        // A bound moved up keeps the requirement ahead of launch's while the band before pays for it.
        (margins(&[(Some(2_000_000), 200), (Some(3_000_000), 100), (Some(10_000_000), 220), (None, 500)]), true),
        (margins(&[(Some(3_000_000), 100), (Some(10_000_000), 200), (None, 500)]), false),
        (margins(&[(None, 500)]), true),
        (margins(&[(None, 499)]), false),
    ];
    for (bands, admitted) in cases {
        let checked = check_margin_at_or_above_launch(&bands, &launch);
        assert_eq!(checked.is_ok(), admitted, "{bands:?}: {checked:?}");
    }
}
