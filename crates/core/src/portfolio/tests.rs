use alloy_primitives::U256;
use serde_json::{Value, json};

use super::requirement::Net;
use super::*;

#[test]
fn a_well_formed_grid_passes() {
    assert_eq!(grid().validate(), Ok(()));
}

#[test]
fn the_grid_refuses_each_malformed_part() {
    let cases: [(&str, Break); 14] = [
        ("floor_share_bps", |g| g.floor_share_bps = 0),
        ("floor_share_bps", |g| g.floor_share_bps = 10_001),
        ("non-empty and distinct", |g| g.groups[0].name.clear()),
        ("non-empty and distinct", |g| g.groups[1].name = "crypto".into()),
        ("at least two members", |g| g.groups[0].members.truncate(1)),
        ("more than one place", |g| g.groups[1].members[0].market = 1),
        ("more than one place", |g| g.groups[0].members[1].market = 1),
        ("factor shock", |g| g.groups[0].factor_shock_bps = 0),
        ("factor shock", |g| g.groups[0].weekend_factor_shock_bps = 419),
        ("factor shock", |g| (g.groups[0].factor_shock_bps, g.groups[0].weekend_factor_shock_bps) = (10_000, 10_001)),
        ("residual shock", |g| g.groups[0].members[0].residual_shock_bps = -1),
        ("residual shock", |g| g.groups[0].members[0].weekend_residual_shock_bps = 19),
        ("loading", |g| g.groups[0].members[0].loading = Fixed::ZERO),
        ("loading", |g| g.groups[0].members[1].loading = -(MAX_LOADING + Fixed::from_raw(1))),
    ];
    for (expected, break_it) in cases {
        let mut broken = grid();
        break_it(&mut broken);
        let refused = broken.validate().expect_err(expected);
        assert!(refused.contains(expected), "{expected}: {refused}");
    }
    // The bounds themselves pass: a zero residual, loadings of ten either way, and full shocks.
    let mut edge = grid();
    edge.groups[0].members[0] =
        GroupMember { loading: MAX_LOADING, residual_shock_bps: 0, ..edge.groups[0].members[0] };
    edge.groups[0].members[1].loading = -MAX_LOADING;
    (edge.groups[1].factor_shock_bps, edge.groups[1].weekend_factor_shock_bps, edge.floor_share_bps) =
        (10_000, 10_000, 10_000);
    assert_eq!(edge.validate(), Ok(()));
}

#[test]
fn listed_members_share_a_class_a_schedule_and_usd() {
    let weekly = json!({ "kind": "weekly", "tz": "America/Chicago", "windows": [{ "open": "Sun 17:00", "close": "Fri 16:00" }] });
    let listings = [
        spec(1, "crypto", json!({ "kind": "always_open" }), None),
        spec(2, "crypto", json!({ "kind": "always_open" }), None),
        spec(30, "commodity", weekly.clone(), None),
        spec(31, "commodity", weekly.clone(), None),
    ];
    let check = |listings: &[MarketSpec]| grid().check_listings(|id| listings.iter().find(|spec| spec.id == id));
    assert_eq!(check(&listings), Ok(()));
    // A member not yet listed counts for nothing, and a group with none listed passes.
    assert_eq!(check(&listings[..3]), Ok(()));
    assert_eq!(check(&listings[..2]), Ok(()));
    // Another class, another schedule, or a quote currency other than USD is refused.
    let mut equity = listings.clone();
    equity[1].asset_class = crate::types::AssetClass::Equity;
    assert!(check(&equity).unwrap_err().contains("markets 1 and 2 differ"));
    let mut hours = listings.clone();
    hours[3] = spec(31, "commodity", json!({ "kind": "always_open" }), None);
    assert!(check(&hours).unwrap_err().contains("markets 30 and 31 differ"));
    let mut euros = listings.clone();
    euros[2] = spec(30, "commodity", weekly, Some(json!({ "currency": "eur", "fx_market": 10 })));
    assert!(check(&euros).unwrap_err().contains("market 30 is not quoted in USD"));
}

#[test]
fn opposite_fractional_notionals_cancel_exactly() {
    // Half a nano-unit long and the same short, each a fraction of the smallest `Fixed` notional,
    // cancel; rounding each notional up first would leave a nano-dollar short.
    assert_eq!(net(&[("0.000000001", "0.5", "1"), ("-0.000000001", "0.25", "2")]), Fixed::ZERO);
    // A tenth of a nano-dollar left over rounds its magnitude up to one, where netting the rounded
    // notionals would give nothing.
    assert_eq!(net(&[("0.000000001", "0.5", "1"), ("-0.000000001", "0.4", "1")]), Fixed::from_raw(1));
    // A short remainder rounds its magnitude up, away from zero, not toward positive infinity.
    assert_eq!(net(&[("-0.000000001", "0.5", "1")]), Fixed::from_raw(-1));
    assert_eq!(net(&[("1", "0.333333333", "3"), ("-1", "1", "1")]), Fixed::from_raw(-1));
}

#[test]
fn negative_loadings_turn_a_term_around() {
    assert_eq!(net(&[("2", "100", "-1")]), Fixed::from_int(-200));
    assert_eq!(net(&[("-2", "100", "-1")]), Fixed::from_int(200));
    // Two shorts with loadings of opposite signs cancel.
    assert_eq!(net(&[("-10", "100", "-0.5"), ("-10", "50", "1")]), Fixed::ZERO);
}

#[test]
fn positions_past_the_input_bound_net_exactly_where_i128_would_overflow() {
    // Five billion units, past `Fixed::MAX_INPUT`, at a billion dollars: each raw product is 5 x 10^46,
    // past i128, and the two cancel to a nano-unit of loading on $5 x 10^18.
    let terms = [("5000000000", "1000000000", "10"), ("-5000000000", "1000000000", "9.999999999")];
    assert_eq!(net(&terms), Fixed::from_int(5_000_000_000));
    // A notional at the top of `Fixed`'s range, ten times loaded, nets past it exactly.
    let mut wide = Net::default();
    wide.add("100000000000000000000".parse().unwrap(), Fixed::from_int(1_000_000_000), MAX_LOADING);
    assert_eq!(wide.rounded(), (false, U256::from(10u64).pow(U256::from(39))));
}

#[test]
fn the_net_matches_an_exact_reference_over_many_terms() {
    // Terms of odd sizes and signs, so no partial sum is round: the reference is the sum of the raw
    // products, worked as integers.
    let terms: Vec<(i128, i128, i128)> = (1..40)
        .map(|i| ((-1i128).pow(i) * (7_919 * i as i128), 104_729 + i as i128, 15_485_863 * (i as i128 % 3 - 1)))
        .collect();
    let mut accumulated = Net::default();
    for (qty, price, loading) in &terms {
        accumulated.add(Fixed::from_raw(*qty), Fixed::from_raw(*price), Fixed::from_raw(*loading));
    }
    let exact: i128 = terms.iter().map(|(qty, price, loading)| qty * price * loading).sum();
    let scale = 1_000_000_000_000_000_000i128;
    let magnitude = (exact.abs() + scale - 1) / scale;
    assert_eq!(accumulated.rounded(), (exact < 0, U256::from(magnitude as u128)));
}

// ── Helpers ──

/// An edit that breaks one part of a grid.
type Break = fn(&mut PortfolioGrid);

/// A crypto pair and a commodity pair, BTC and ETH and gold and silver.
fn grid() -> PortfolioGrid {
    let member = |market, loading: &str, residual| GroupMember {
        market,
        loading: loading.parse().unwrap(),
        residual_shock_bps: residual,
        weekend_residual_shock_bps: residual,
    };
    let group = |name: &str, members| OffsetGroup {
        name: name.into(),
        factor_shock_bps: 420,
        weekend_factor_shock_bps: 420,
        members,
    };
    PortfolioGrid {
        floor_share_bps: 5_000,
        groups: vec![
            group("crypto", vec![member(1, "1", 20), member(2, "1.05", 20)]),
            group("metals", vec![member(30, "1", 50), member(31, "-0.8", 80)]),
        ],
    }
}

/// A listing of `id` in `class` on `schedule`, quoted in `currency` or USD.
fn spec(id: MarketId, class: &str, schedule: Value, currency: Option<Value>) -> MarketSpec {
    let mut spec = json!({
        "id": id, "symbol": format!("M{id}"), "asset_class": class, "schedule": schedule,
        "margin_tiers": [{ "bps": 100 }], "spread_tiers": [{ "bps": 0 }],
        "price_rules": { "max_age_ms": 1000, "max_divergence_bps": 10, "min_sources": 2 },
        "max_slippage_bps": 10, "financing": { "benchmark": "SOFR", "markup_bps": 250, "day_basis": 360 },
        "house_cap_bps": 3000,
    });
    if let Some(currency) = currency {
        spec["quote_currency"] = currency;
    }
    serde_json::from_value(spec).unwrap()
}

/// The signed net of `terms`, each (qty, price, loading), as the account read gives it.
fn net(terms: &[(&str, &str, &str)]) -> Fixed {
    let mut net = Net::default();
    for (qty, price, loading) in terms {
        net.add(qty.parse().unwrap(), price.parse().unwrap(), loading.parse().unwrap());
    }
    let (short, magnitude) = net.rounded();
    let magnitude = Fixed::from_raw(i128::try_from(magnitude).unwrap());
    if short { -magnitude } else { magnitude }
}
