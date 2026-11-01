use engine_core::price::SourceQuote;
use engine_core::types::Side;

use super::*;

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
