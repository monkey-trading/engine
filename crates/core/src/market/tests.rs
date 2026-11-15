use super::*;

fn f(n: i64) -> Fixed {
    Fixed::from_int(n)
}

fn tiers() -> Vec<Tier> {
    vec![
        Tier { up_to: Some(f(1_000_000)), bps: 0 },
        Tier { up_to: Some(f(5_000_000)), bps: 2 },
        Tier { up_to: None, bps: 5 },
    ]
}

#[test]
fn staircase_charges_each_slice_at_its_band_rate() {
    let t = tiers();
    let cost = |from, to| staircase(&t, from, to, Rounding::TowardZero);
    assert_eq!(cost(f(0), f(500_000)), f(0));
    // 1m..2m at 2bps.
    assert_eq!(cost(f(0), f(2_000_000)), f(200));
    // 1m..5m at 2bps = 800, 5m..6m at 5bps = 500.
    assert_eq!(cost(f(0), f(6_000_000)), f(1_300));
    assert_eq!(cost(f(4_000_000), f(6_000_000)), f(700));
    // A shrinking move costs nothing.
    assert_eq!(cost(f(6_000_000), f(4_000_000)), f(0));
}

#[test]
fn staircase_rounds_each_band_charge_as_asked() {
    let t = tiers();
    // One unit of precision either side of 5m charges a fraction of a unit in each band, which
    // truncates to nothing and rounds up to one unit per band. An exact charge does not move.
    let (from, to) = (f(4_999_999) + "0.999999999".parse().unwrap(), f(5_000_000) + Fixed::from_raw(1));
    assert_eq!(staircase(&t, from, to, Rounding::TowardZero), Fixed::ZERO);
    assert_eq!(staircase(&t, from, to, Rounding::Up), Fixed::from_raw(2));
    let half = f(1_000_000) + "0.5".parse().unwrap();
    assert_eq!(staircase(&t, f(0), half, Rounding::Up), "0.0001".parse().unwrap());
}

#[test]
fn splitting_does_not_avoid_size_spread() {
    let t = tiers();
    let cost = |from, to| staircase(&t, from, to, Rounding::TowardZero);
    assert_eq!(cost(f(0), f(6_000_000)), cost(f(0), f(3_000_000)) + cost(f(3_000_000), f(6_000_000)));
}

#[test]
fn tier_validation() {
    assert!(validate_tiers("t", &tiers(), 0).is_ok());
    // A zero rate is fine for spreads but not for margin.
    assert!(validate_tiers("t", &tiers(), 1).is_err());
    assert!(validate_tiers("t", &[], 0).is_err());
    assert!(validate_tiers("t", &[Tier { up_to: Some(f(1)), bps: 1 }], 0).is_err());
    let backwards =
        vec![Tier { up_to: Some(f(5)), bps: 1 }, Tier { up_to: Some(f(2)), bps: 1 }, Tier { up_to: None, bps: 1 }];
    assert!(validate_tiers("t", &backwards, 0).is_err());
    let huge = vec![Tier { up_to: None, bps: i64::MAX }];
    assert!(validate_tiers("t", &huge, 0).is_err());
}

#[test]
fn unknown_tier_fields_are_rejected() {
    assert!(serde_json::from_str::<Tier>(r#"{"up_to":"1","bps":1,"bsp":2}"#).is_err());
}

const EUR: QuoteCurrency = QuoteCurrency::Eur { fx_market: 10 };
const JPY: QuoteCurrency = QuoteCurrency::Jpy { fx_market: 12 };

fn d(s: &str) -> Fixed {
    s.parse().unwrap()
}

#[test]
fn euros_multiply_and_yen_divide_by_the_fx_index_rounding_the_way_asked() {
    use Rounding::{Down, Up};
    // (currency, amount, index, rounded down, rounded up): a gain and a loss through a USD-quote
    // pair (EUR-USD, times the index) and a USD-base pair (USD-JPY, over the index).
    let cases = [
        (EUR, "0.000000003", "1.5", "0.000000004", "0.000000005"),
        (EUR, "-0.000000003", "1.5", "-0.000000005", "-0.000000004"),
        (EUR, "200", "1.08", "216", "216"),
        (JPY, "100", "150", "0.666666666", "0.666666667"),
        (JPY, "-100", "150", "-0.666666667", "-0.666666666"),
        (JPY, "-300", "150", "-2", "-2"),
    ];
    for (currency, amount, fx, down, up) in cases {
        let convert = |rounding| currency.to_usd(d(amount), Some(d(fx)), rounding);
        assert_eq!((convert(Down), convert(Up)), (d(down), d(up)), "{currency:?} {amount} at {fx}");
    }
    assert_eq!(EUR.from_usd(d("1"), Some(d("3")), Down), d("0.333333333"));
    assert_eq!(EUR.from_usd(d("1"), Some(d("3")), Up), d("0.333333334"));
    assert_eq!(JPY.from_usd(d("0.000000003"), Some(d("1.5")), Up), d("0.000000005"));
    assert_eq!(JPY.from_usd(d("2"), Some(d("150")), Down), d("300"));
    let usd = QuoteCurrency::Usd;
    assert_eq!((usd.to_usd(d("7"), None, Up), usd.from_usd(d("7"), None, Down)), (d("7"), d("7")));
}

#[test]
fn a_market_with_no_fx_index_converts_only_zero() {
    assert_eq!(EUR.to_usd(Fixed::ZERO, None, Rounding::Down), Fixed::ZERO);
    assert_eq!(JPY.from_usd(Fixed::ZERO, None, Rounding::Up), Fixed::ZERO);
    let converted = std::panic::catch_unwind(|| EUR.to_usd(d("1"), None, Rounding::Down));
    assert!(converted.is_err(), "a non-zero amount without an index must not convert");
}

#[test]
fn an_fx_index_converts_while_a_unit_is_worth_at_most_ten_dollars() {
    assert!(EUR.converts_at(d("10")));
    assert!(!EUR.converts_at(d("10.000000001")));
    // A yen is worth 1 / index USD, so a USD-JPY index converts from 0.1 up.
    assert!(JPY.converts_at(d("0.1")));
    assert!(!JPY.converts_at(d("0.099999999")));
    assert!(JPY.converts_at(Fixed::MAX_INPUT));
    assert!(EUR.converts_at(Fixed::from_raw(1)));
}

#[test]
fn a_usd_listing_leaves_the_currency_out_and_others_name_their_fx_market() {
    let spec = |extra: serde_json::Value| {
        let mut json = serde_json::json!({
            "id": 23, "symbol": "DE40", "asset_class": "index", "schedule": { "kind": "always_open" },
            "margin_tiers": [{ "bps": 50 }], "spread_tiers": [{ "bps": 0 }],
            "price_rules": { "max_age_ms": 1000, "max_divergence_bps": 10, "min_sources": 2 },
            "max_slippage_bps": 10, "financing": { "benchmark": "ESTR", "markup_bps": 250, "day_basis": 360 },
            "house_cap_bps": 5000,
        });
        json.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value::<MarketSpec>(json)
    };
    let usd = spec(serde_json::json!({})).unwrap();
    assert_eq!(usd.quote_currency, QuoteCurrency::Usd);
    assert!(!serde_json::to_string(&usd).unwrap().contains("quote_currency"));
    let de40 = spec(serde_json::json!({ "quote_currency": { "currency": "eur", "fx_market": 10 } })).unwrap();
    assert_eq!(de40.quote_currency, EUR);
    assert_eq!(de40.quote_currency.fx_market(), Some(10));
    let encoded = serde_json::to_string(&de40).unwrap();
    assert!(encoded.contains(r#""quote_currency":{"currency":"eur","fx_market":10}"#), "{encoded}");
    for bad in [
        serde_json::json!({ "quote_currency": { "currency": "eur" } }),
        serde_json::json!({ "quote_currency": { "currency": "chf", "fx_market": 10 } }),
        serde_json::json!({ "quote_currency": { "currency": "eur", "fx_market": 10, "fx": 1 } }),
    ] {
        assert!(spec(bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn a_weekend_margin_raises_each_band_to_at_least_its_rate() {
    let at = |weekend_margin_bps, notional: &str| {
        commodity(weekend_margin_bps, weekly()).weekend_margin_for(d(notional), Rounding::Up)
    };
    // 1% to 2m and 5% above: 7.5% raises both bands, 3% only the first, and none leaves them.
    assert_eq!((at(750, "1000000"), at(750, "-3000000")), (d("75000"), d("225000")));
    assert_eq!((at(300, "1000000"), at(300, "3000000")), (d("30000"), d("110000")));
    // Under every band it charges nothing extra.
    let under = commodity(50, weekly());
    assert_eq!(under.weekend_margin_for(d("3000000"), Rounding::Up), under.margin_for(d("3000000"), Rounding::Up));
    let none = commodity(0, weekly());
    assert_eq!(none.weekend_margin_for(d("3000000"), Rounding::Up), none.margin_for(d("3000000"), Rounding::Up));
}

#[test]
fn a_weekend_margin_is_a_rate_to_100_percent_on_a_weekly_schedule() {
    // A rate under a band leaves that band's charge as it is, so any rate is at or above the normal
    // margin.
    for bps in [0, 50, 750, 10_000] {
        assert_eq!(commodity(bps, weekly()).validate(), Ok(()), "{bps}");
    }
    for bps in [-1, 10_001] {
        assert_eq!(
            commodity(bps, weekly()).validate().unwrap_err(),
            "weekend_margin_bps must be in 0..=10000",
            "{bps}"
        );
    }
    let always = commodity(750, serde_json::json!({ "kind": "always_open" }));
    assert_eq!(always.validate().unwrap_err(), "weekend_margin_bps needs a weekly schedule");
    // Left out of the encoding while none, so earlier listings keep their roots.
    assert!(!serde_json::to_string(&commodity(0, weekly())).unwrap().contains("weekend_margin_bps"));
    assert!(serde_json::to_string(&commodity(750, weekly())).unwrap().contains(r#""weekend_margin_bps":750"#));
}

/// An oil listing at 1% margin to 2m and 5% above, on `schedule`, with `weekend_margin_bps`.
fn commodity(weekend_margin_bps: i64, schedule: serde_json::Value) -> MarketSpec {
    serde_json::from_value(serde_json::json!({
        "id": 32, "symbol": "WTI", "asset_class": "commodity", "schedule": schedule,
        "margin_tiers": [{ "up_to": "2000000", "bps": 100 }, { "bps": 500 }], "spread_tiers": [{ "bps": 0 }],
        "price_rules": { "max_age_ms": 1000, "max_divergence_bps": 10, "min_sources": 2 },
        "max_slippage_bps": 10, "financing": { "benchmark": "SOFR", "markup_bps": 250, "day_basis": 360 },
        "house_cap_bps": 3000, "weekend_margin_bps": weekend_margin_bps,
    }))
    .unwrap()
}

/// Futures hours in Chicago, Sunday 17:00 to Friday 16:00.
fn weekly() -> serde_json::Value {
    serde_json::json!({ "kind": "weekly", "tz": "America/Chicago", "windows": [{ "open": "Sun 17:00", "close": "Fri 16:00" }] })
}
