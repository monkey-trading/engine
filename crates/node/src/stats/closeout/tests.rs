use super::*;
use crate::test_support::dev_config;

fn f(n: i64) -> Fixed {
    Fixed::from_int(n)
}

/// BTC-USD of the dev genesis: 10% margin on the first million of notional and 20% above.
fn btc() -> MarketSpec {
    dev_config().genesis.markets.into_iter().find(|market| market.id == 1).unwrap()
}

fn standing(equity: i64, margin: i64) -> Standing {
    Standing { equity: f(equity), margin: f(margin), level_bps: 5_000 }
}

#[test]
fn a_close_out_across_a_margin_band_is_found_where_the_slack_reaches_zero() {
    let spec = btc();
    // A long of 1.5M with 1M of equity and a 200k margin (10% of the first million, 20% of the rest),
    // closed out at half the margin. Falling to notional N the equity is 1M + (N - 1.5M) and the
    // margin 10% of N below the million, so the slack N - 0.5M - 0.05N is nil at N = 526316: 64.9%
    // below the mark, in the first band although the position stands in the second.
    let position = Position { notional: f(1_500_000), long: true, spec: &spec };
    let reach = reach(standing(1_000_000, 200_000), position, 9_000);
    let Reach::At(offset) = reach else { panic!("{reach:?}") };
    assert!((-6_500..=-6_480).contains(&offset), "{offset}");
    // The same account's slack at the estimate is nil: its equity is the level's share of the margin.
    let notional = f(1_500_000) + f(1_500_000).mul_bps(offset);
    let equity = f(1_000_000) - (f(1_500_000) - notional);
    let margin = spec.margin_for(notional, Rounding::Up);
    assert!((equity - margin.mul_bps(5_000)).abs() < f(200), "{equity} vs {margin}");
}

#[test]
fn a_short_closes_out_on_a_rise_and_an_account_already_below_the_level_is_due() {
    let spec = btc();
    // A short of 500k on 100k of equity and a 50k margin: closed out at a notional of 571429.
    let position = Position { notional: f(500_000), long: false, spec: &spec };
    let Reach::At(offset) = reach(standing(100_000, 50_000), position, 5_000) else { panic!() };
    assert!(offset > 0 && offset <= 5_000, "{offset}");
    let due = reach(standing(20_000, 50_000), position, 5_000);
    assert_eq!(due, Reach::Due, "equity under half the margin is closed out at the marks");
}

#[test]
fn a_position_no_move_in_range_closes_out_is_beyond_it() {
    let spec = btc();
    let position = Position { notional: f(100_000), long: true, spec: &spec };
    assert_eq!(reach(standing(1_000_000, 10_000), position, 2_000), Reach::Beyond);
}

#[test]
fn a_self_converting_pair_closes_out_where_its_yen_profit_in_dollars_falls_to_the_level() {
    // 100000 USD of USD-JPY long at 150, on 10000 of equity and a 1000 margin: closed out at half the
    // margin, 500, so 9500 of USD profit is lost. USD profit is 100000 * 150 * (1/p - 1/150) negated.
    let (quantity, entry, mark) = (f(100_000), f(150), f(150));
    let Reach::At(long) = reach_self_converting(standing(10_000, 1_000), true, quantity, entry, mark, 2_000) else {
        panic!()
    };
    // 1/p = 1/150 + 9500 / (100000 * 150), so p = 136.99: 868 bps below the mark.
    assert!((-870..=-866).contains(&long), "{long}");
    let Reach::At(short) = reach_self_converting(standing(10_000, 1_000), false, quantity, entry, mark, 2_000) else {
        panic!()
    };
    // 1/p = 1/150 - 9500 / (100000 * 150), so p = 165.76: 1050 bps above.
    assert!((1_045..=1_055).contains(&short), "{short}");
    assert_eq!(reach_self_converting(standing(10_000, 1_000), true, quantity, entry, mark, 500), Reach::Beyond);
    assert_eq!(reach_self_converting(standing(400, 1_000), true, quantity, entry, mark, 2_000), Reach::Due);
}
