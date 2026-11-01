use engine_core::command::AdminCommand;
use engine_core::price::SourceQuote;
use engine_core::types::Side;

use super::*;
use crate::test_support::dev_config;

const EUR_USD: MarketId = 10;
const DE40: MarketId = 23;

fn quote(market: MarketId, mid: &str, ts: Millis) -> Command {
    let quotes = ["a", "b"]
        .map(|source| SourceQuote { source: source.into(), mid: mid.parse().unwrap(), ts, contract: None })
        .to_vec();
    Command::Prices { market, quotes }
}

/// A market quoted in euros counts in dollars at the EUR-USD index, in the volume and in the scan.
#[test]
fn a_market_quoted_in_euros_is_valued_in_dollars_at_the_fx_index() {
    let mut rig = Rig::new();
    rig.apply(T0 + 500, quote(EUR_USD, "1.08", T0 + 500));
    rig.apply(T0 + 500, quote(DE40, "23500", T0 + 500));
    let events = rig.trade_in(DE40, T0 + 600, TRADER, Side::Buy, 1);
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    // One contract at 23500 EUR is 25380 USD, give or take the spread.
    let expected = Fixed::from_int(25_380);
    let near = |value: Fixed| (value - expected).abs() < Fixed::from_int(100);
    let query = crate::stats::SeriesQuery {
        metric: "volume".into(),
        market: Some(DE40),
        resolution: "1m".into(),
        from_ms: None,
        to_ms: None,
        limit: None,
    };
    let series = serde_json::to_value(rig.stats.series(&rig.engine, &query).unwrap().finish()).unwrap();
    assert!(near(series["entries"][0]["value"].as_str().unwrap().parse().unwrap()), "{series}");
    let snapshot = crate::stats::snapshot::scan(&rig.engine, 0, T0, crate::stats::tests::closeout_map::layout(), true);
    assert!(near(snapshot.markets[&DE40].long.usd), "{:?}", snapshot.markets[&DE40].long);
}

/// USD-JPY converts at its own index, so a fall that costs a long its margin is not the linear one
/// the other markets give: the scan's bucket for a position there is where the engine closes it out.
fn usd_jpy_agrees_with_the_engine(side: Side) {
    use crate::stats::snapshot::{Grid, Layout, scan};
    const USD_JPY: MarketId = 12;
    let production = include_str!("../../../../../config/production-genesis.toml");
    let genesis: toml::Table = toml::from_str(production).unwrap();
    let listing = genesis["genesis"]["markets"].as_array().unwrap().iter().find(|m| m["id"].as_integer() == Some(12));
    let spec: engine_core::market::MarketSpec = listing.unwrap().clone().try_into().unwrap();
    let layout = Layout { closeout: Grid::new(200, 10), profit: Grid::new(5_000, 500) };
    let long = side == Side::Buy;
    let build = || {
        let mut rig = Rig::new();
        let operator = dev_config().operator().unwrap();
        let key = k256::ecdsa::SigningKey::from_slice(&[0x42; 32]).unwrap();
        let nonce = rig.engine.next_request_nonce(operator.address);
        let listed = crate::admin::sign(&key, &operator, AdminCommand::UpsertMarket(spec.clone()), nonce, T0 + 60_000);
        let events = rig.apply(T0 + 100, Command::SignedAdmin(Box::new(listed)));
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
        rig.price_in(USD_JPY, T0 + 200, Fixed::from_int(150));
        let events = rig.trade_in(USD_JPY, T0 + 300, TRADER, side, 100_000);
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
        // Leave the account 1500 of equity behind a 500 margin: closed out at 250.
        let mut state = serde_json::to_value(&rig.engine).unwrap();
        state["accounts"][TRADER.to_string()]["balance"] = serde_json::json!("1500");
        rig.engine = serde_json::from_value(state).unwrap();
        rig
    };
    let rig = build();
    let book = &scan(&rig.engine, 0, T0, layout, true).markets[&USD_JPY];
    let held = |bin: &crate::stats::snapshot::Bin| if long { bin.long.count } else { bin.short.count };
    let bucket = book.closeout.iter().position(|bin| held(bin) == 1).expect("the position closes out in range");
    let (width, half) = (i64::from(layout.closeout.width), i64::from(layout.closeout.half));
    let (from, to) = ((bucket as i64 - half) * width, (bucket as i64 - half + 1) * width);
    let price_at = |bps: i64| Fixed::from_int(150) + Fixed::from_int(150).mul_bps(bps);
    let (near, far) = if long {
        (price_at(to + width), price_at(from - width))
    } else {
        (price_at(from - width), price_at(to + width))
    };
    let closes_at = |target: Fixed| {
        let mut rig = build();
        let (mut price, mut at, mut closed) = (Fixed::from_int(150), T0 + 400, false);
        let step = Fixed::from_raw(20_000_000);
        while price != target && !closed {
            let toward = if target > price { (target - price).min(step) } else { -(price - target).min(step) };
            price += toward;
            closed = rig.price_in(USD_JPY, at, price).iter().any(|event| matches!(event, Event::CloseOut { .. }));
            at += 1_000;
        }
        closed
    };
    assert!(!closes_at(near), "no close-out at {near}, short of the estimate's {from}..{to} bps");
    assert!(closes_at(far), "a close-out at {far}, past the estimate's {from}..{to} bps");
}

#[test]
fn a_long_in_the_market_that_converts_its_own_currency_closes_out_where_the_scan_says() {
    usd_jpy_agrees_with_the_engine(Side::Buy);
}

#[test]
fn a_short_in_the_market_that_converts_its_own_currency_closes_out_where_the_scan_says() {
    usd_jpy_agrees_with_the_engine(Side::Sell);
}
