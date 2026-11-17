use alloy_primitives::U512;
use serde_json::{Value, json};

use super::exposure::{Exposure, Leg};
use super::*;

/// The dev fixture's grid charge on each dollar of notional, over `CHARGE_SCALE`: a 20bp residual at a
/// 50% close-out level, 20 × 5000, and a 30bp ceiling, 30 × 10^4, so 0.2% plus 0.6%.
const PER_UNIT: u64 = 20 * 5_000 + 30 * 10_000;
/// 10^4 × a close-out level of 5000 bps.
const CHARGE_SCALE: u64 = 10_000 * 5_000;

#[test]
fn a_well_formed_grid_passes() {
    assert_eq!(grid().validate(), Ok(()));
}

#[test]
fn the_grid_refuses_each_malformed_part() {
    let cases: [(&str, Break); 11] = [
        ("floor_share_bps", |g| g.groups[0].floor_share_bps = 0),
        ("floor_share_bps", |g| g.groups[1].floor_share_bps = 10_001),
        ("non-empty and distinct", |g| g.groups[0].name.clear()),
        ("non-empty and distinct", |g| g.groups[1].name = "crypto".into()),
        ("at least two members", |g| g.groups[0].members.truncate(1)),
        ("more than one place", |g| g.groups[1].members[0].market = 1),
        ("more than one place", |g| g.groups[0].members[1].market = 1),
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
    // The bounds themselves pass: a zero residual, loadings of ten either way, full shocks and shares.
    let mut edge = grid();
    edge.groups[0].members[0] =
        GroupMember { loading: MAX_LOADING, residual_shock_bps: 0, ..edge.groups[0].members[0] };
    edge.groups[0].members[1] = GroupMember {
        loading: -MAX_LOADING,
        residual_shock_bps: 10_000,
        weekend_residual_shock_bps: 10_000,
        ..edge.groups[0].members[1]
    };
    (edge.groups[0].floor_share_bps, edge.groups[1].floor_share_bps) = (1, 10_000);
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
    let mut wide = Exposure::default();
    wide.add(leg("100000000000000000000", "1000000000", "10", "0", 0));
    assert_eq!(wide.net(), (false, U512::from(10u64).pow(U512::from(39))));
}

#[test]
fn the_net_matches_an_exact_reference_over_many_terms() {
    // Terms of odd sizes and signs, so no partial sum is round: the reference is the sum of the raw
    // products, worked as integers.
    let terms: Vec<(i128, i128, i128)> = (1..40)
        .map(|i| ((-1i128).pow(i) * (7_919 * i as i128), 104_729 + i as i128, 15_485_863 * (i as i128 % 3 - 1)))
        .collect();
    let mut accumulated = Exposure::default();
    for (qty, price, loading) in &terms {
        let leg = Leg {
            qty: Fixed::from_raw(*qty),
            price: Fixed::from_raw(*price),
            loading: Fixed::from_raw(*loading),
            bands: Fixed::ZERO,
            charge: U512::ZERO,
            residual: U512::ZERO,
        };
        accumulated.add(leg);
    }
    let exact: i128 = terms.iter().map(|(qty, price, loading)| qty * price * loading).sum();
    let scale = 1_000_000_000_000_000_000i128;
    let magnitude = (exact.abs() + scale - 1) / scale;
    assert_eq!(accumulated.net(), (exact < 0, U512::from(magnitude as u128)));
}

#[test]
fn exposure_on_one_side_gets_no_credit() {
    // A lone leg, two legs on one side through loadings of opposite signs, and an empty book.
    let lone = exposure([leg("10", "100000", "1", "10000", PER_UNIT)]);
    let one_side =
        exposure([leg("10", "100000", "1", "10000", PER_UNIT), leg("-250", "4000", "-1", "10000", PER_UNIT)]);
    for book in [lone, one_side, Exposure::default()] {
        assert_eq!(
            (book.credit(5_000, U512::from(CHARGE_SCALE)), book.matched(), book.worst_loss()),
            (Fixed::ZERO, U512::ZERO, U512::ZERO)
        );
    }
}

#[test]
fn the_credit_is_the_matched_parts_bands_less_its_charge_rounded_down_once() {
    // $1M of BTC long against $1M of ETH short loading 1.05: BTC is matched whole and ETH in the ratio
    // 1/1.05. The matched bands are 10,000 + 10,000/1.05 and the grid charge 0.8% of the matched
    // notional, so the credit is 2,000 + 2,000/1.05 = 3,904.7619047619..., rounded down once. Rounding
    // the ratio first, to 0.952380952, would give 3,904.761904.
    let book = exposure([leg("10", "100000", "1", "10000", PER_UNIT), leg("-250", "4000", "1.05", "10000", PER_UNIT)]);
    let scale = U512::from(CHARGE_SCALE);
    assert_eq!(book.credit(5_000, scale), "3904.761904761".parse().unwrap());
    // A 90% floor share keeps a tenth of the matched bands, 1,952.38095238..., rounded down.
    assert_eq!(book.credit(9_000, scale), "1952.380952380".parse().unwrap());
    // The worst loss is the matched part's 0.2%, rounded up, and twice the smaller side is matched.
    assert_eq!(book.worst_loss(), U512::from(3_904_761_904_762u64));
    assert_eq!(book.matched(), U512::from(2_000_000_000_000_000u64));
    // A grid charge at or above the matched bands credits nothing.
    let costly = exposure([leg("10", "100000", "1", "10000", 2_000_000), leg("-250", "4000", "1", "10000", 2_000_000)]);
    assert_eq!(costly.credit(1, scale), Fixed::ZERO);
}

#[test]
fn a_leg_left_out_of_the_credit_still_matches_its_side() {
    // BTC brings no bands and no charge, as a market with no weekend rate does over its weekend, yet it
    // still matches ETH in the ratio 1/1.05: the credit is ETH's matched part alone, 2,000/1.05, rounded
    // down once.
    let book = exposure([leg("10", "100000", "1", "0", 0), leg("-250", "4000", "1.05", "10000", PER_UNIT)]);
    assert_eq!(book.credit(5_000, U512::from(CHARGE_SCALE)), "1904.761904761".parse().unwrap());
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
    let group = |name: &str, members| OffsetGroup { name: name.into(), floor_share_bps: 5_000, members };
    PortfolioGrid {
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

/// A leg of `qty` at `price` with `loading`, `bands` of per-position requirement, a grid charge of
/// `per_unit` on each dollar of notional over `CHARGE_SCALE`, and a 20bp residual.
fn leg(qty: &str, price: &str, loading: &str, bands: &str, per_unit: u64) -> Leg {
    let (qty, price): (Fixed, Fixed) = (qty.parse().unwrap(), price.parse().unwrap());
    let notional = U512::from(qty.abs().mul(price).raw().unsigned_abs());
    Leg {
        qty,
        price,
        loading: loading.parse().unwrap(),
        bands: bands.parse().unwrap(),
        charge: notional * U512::from(per_unit),
        residual: notional * U512::from(20u64),
    }
}

/// The exposure of `legs`.
fn exposure(legs: impl IntoIterator<Item = Leg>) -> Exposure {
    let mut exposure = Exposure::default();
    legs.into_iter().for_each(|leg| exposure.add(leg));
    exposure
}

/// The signed net of `terms`, each (qty, price, loading), as the account read gives it.
fn net(terms: &[(&str, &str, &str)]) -> Fixed {
    let book = exposure(terms.iter().map(|(qty, price, loading)| leg(qty, price, loading, "0", 0)));
    let (short, magnitude) = book.net();
    let magnitude = Fixed::from_raw(i128::try_from(magnitude).unwrap());
    if short { -magnitude } else { magnitude }
}
