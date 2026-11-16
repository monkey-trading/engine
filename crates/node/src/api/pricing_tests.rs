//! `pricing` over the engine's own state: a market priced by two sources, one lagging, and recovery.

use engine_core::Engine;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Millis;
use serde_json::json;

use super::pricing::{Pricing, Unpriced, pricing};
use crate::feeds::status::{Connection, FeedStatus, SymbolState, SymbolStatus};
use crate::status::{Cause, Status};
use crate::test_support::dev_config;
use crate::trusted_time::{HeldCause, TimeState};

const T0: Millis = 1_792_504_800_000;

fn apply(engine: &mut Engine, ts: Millis, cmd: Command) -> Vec<Event> {
    let events = engine.apply(&Sequenced { seq: engine.last_seq().map_or(0, |seq| seq + 1), ts, cmd }).unwrap();
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    events
}

/// An engine listing market 1 of the dev genesis (`max_age_ms` 1000, `min_sources` 2).
fn engine() -> Engine {
    let mut engine = Engine::new();
    let config = dev_config();
    apply(&mut engine, T0, Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)));
    apply(&mut engine, T0, Command::Admin(AdminCommand::SetOperator(config.operator().unwrap())));
    apply(&mut engine, T0, Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V22 }));
    apply(&mut engine, T0, Command::Admin(AdminCommand::SetParams(config.genesis.params)));
    let market = config.genesis.markets.into_iter().find(|market| market.id == 1).unwrap();
    apply(&mut engine, T0, Command::Admin(AdminCommand::UpsertMarket(market)));
    engine
}

/// Quotes from each of `quotes`' sources at `mid`, stamped `ts`.
fn quote(engine: &mut Engine, ts: Millis, quotes: &[(&str, &str)]) {
    let quotes = quotes
        .iter()
        .map(|(source, mid)| SourceQuote { source: (*source).into(), mid: mid.parse().unwrap(), ts, contract: None })
        .collect();
    apply(engine, ts, Command::Prices { market: 1, quotes });
}

fn read(engine: &Engine) -> serde_json::Value {
    let view: Pricing = pricing(&engine.markets()[&1], engine.now(), &[]);
    serde_json::to_value(view).unwrap()
}

#[test]
fn a_market_priced_by_two_sources_loses_its_price_when_one_lags_and_regains_it_on_recovery() {
    let mut engine = engine();
    quote(&mut engine, T0 + 100, &[("coinbase", "100000"), ("kraken", "100000")]);
    let priced = read(&engine);
    assert_eq!(
        priced,
        json!({
            "priced": true, "as_of_ms": T0 + 100, "fresh_sources": 2, "min_sources": 2, "max_age_ms": 1000,
            "max_divergence_bps": 25, "divergence_bps": 0,
            "sources": [
                { "source": "coinbase", "state": "counts", "last_quote_ms": T0 + 100, "age_ms": 0 },
                { "source": "kraken", "state": "counts", "last_quote_ms": T0 + 100, "age_ms": 0 },
            ],
        })
    );
    // Kraken's stream runs behind: only Coinbase's quote is sequenced, and its newest is past the bound.
    quote(&mut engine, T0 + 4_100, &[("coinbase", "100000")]);
    let lagging = read(&engine);
    assert_eq!((&lagging["priced"], &lagging["unpriced"]), (&json!(false), &json!("too_few_sources")));
    assert_eq!((&lagging["fresh_sources"], &lagging["min_sources"]), (&json!(1), &json!(2)));
    assert_eq!(
        lagging["sources"][1],
        json!({ "source": "kraken", "state": "stale", "last_quote_ms": T0 + 100, "age_ms": 4_000 })
    );
    assert_eq!(lagging["sources"][0]["state"], "counts");
    // It quotes again.
    quote(&mut engine, T0 + 4_300, &[("kraken", "100000")]);
    let recovered = read(&engine);
    assert_eq!((&recovered["priced"], &recovered["fresh_sources"]), (&json!(true), &json!(2)));
    assert!(recovered.get("unpriced").is_none(), "{recovered}");
}

#[test]
fn fresh_sources_too_far_apart_are_diverging() {
    let mut engine = engine();
    quote(&mut engine, T0 + 100, &[("coinbase", "100000"), ("kraken", "100500")]);
    let view = read(&engine);
    assert_eq!(
        (&view["priced"], &view["unpriced"], &view["fresh_sources"]),
        (&json!(false), &json!("diverging"), &json!(2))
    );
    assert_eq!(view["divergence_bps"], 49);
    assert_eq!(view["sources"][0]["state"], "diverging");
    assert_eq!(view["sources"][1]["state"], "diverging");
    assert_eq!(pricing(&engine.markets()[&1], engine.now(), &[]).unpriced, Some(Unpriced::Diverging));
}

fn symbol(state: SymbolState) -> Connection {
    Connection { symbols: vec![SymbolStatus { symbol: "BTC-USD".into(), market: 1, contract: None, state }] }
}

#[test]
fn a_source_names_what_its_connection_reports_and_a_venue_that_never_quoted_is_listed() {
    let mut engine = engine();
    quote(&mut engine, T0 + 100, &[("coinbase", "100000"), ("kraken", "100000")]);
    quote(&mut engine, T0 + 4_100, &[("coinbase", "100000")]);
    let feeds = FeedStatus::default();
    feeds.publish("kraken", 0, symbol(SymbolState::Delayed));
    feeds.publish("bitstamp", 0, symbol(SymbolState::Down));
    feeds.publish("coinbase", 0, symbol(SymbolState::Quoting));
    let view = serde_json::to_value(pricing(&engine.markets()[&1], engine.now(), &feeds.venues())).unwrap();
    let states: Vec<_> =
        view["sources"].as_array().unwrap().iter().map(|s| (s["source"].as_str().unwrap(), &s["state"])).collect();
    assert_eq!(states, [("coinbase", &json!("counts")), ("kraken", &json!("delayed")), ("bitstamp", &json!("down")),]);
    assert_eq!(view["sources"][2]["last_quote_ms"], json!(null));
    // The venue recovers: its quote counts again and the report's cause no longer applies.
    feeds.publish("kraken", 0, symbol(SymbolState::Quoting));
    quote(&mut engine, T0 + 4_300, &[("kraken", "100000")]);
    let view = serde_json::to_value(pricing(&engine.markets()[&1], engine.now(), &feeds.venues())).unwrap();
    assert_eq!((&view["priced"], &view["sources"][1]["state"]), (&json!(true), &json!("counts")));
}

#[test]
fn the_status_names_what_stops_orders_and_the_open_markets_with_no_price() {
    let mut engine = engine();
    quote(&mut engine, T0 + 100, &[("coinbase", "100000"), ("kraken", "100000")]);
    quote(&mut engine, T0 + 4_100, &[("coinbase", "100000")]);
    let gauge = prometheus_client::metrics::gauge::Gauge::default();
    let status = Status { log_sync_held: Some(gauge.clone()), ..Default::default() };
    status.feeds.publish("kraken", 0, symbol(SymbolState::Delayed));
    let healthy = status.view(&engine, TimeState::Sampled { last_sample_ms: T0 });
    assert!(healthy.takes_orders && healthy.holds.is_empty());
    assert_eq!(
        healthy.venues.iter().map(|v| (v.venue.as_str(), v.state)).collect::<Vec<_>>(),
        [("kraken", SymbolState::Delayed)]
    );
    assert_eq!(
        healthy
            .unpriced_markets
            .iter()
            .map(|m| (m.market, m.reason, m.fresh_sources, m.min_sources))
            .collect::<Vec<_>>(),
        [(1, Unpriced::TooFewSources, 1, 2)]
    );
    gauge.set(1);
    let held = status.view(&engine, TimeState::Held { cause: HeldCause::NoSample, last_sample_ms: T0 });
    assert!(!held.takes_orders);
    assert_eq!(held.holds.iter().map(|h| h.cause).collect::<Vec<_>>(), [Cause::TrustedTime, Cause::LogSyncHeld]);
    assert!(held.holds.iter().all(|h| h.since_ms > 0));
    gauge.set(0);
    assert!(status.view(&engine, TimeState::Sampled { last_sample_ms: T0 }).takes_orders);
}

#[test]
fn a_rollback_latch_set_by_the_poster_shows_as_a_hold_from_the_first_read_that_sees_it() {
    let engine = engine();
    let latch = crate::log_heads::FreezeLatch::default();
    let status = Status { rollback: Some(latch.clone()), ..Default::default() };
    assert!(status.view(&engine, TimeState::Starting).takes_orders);
    latch.freeze();
    let view = status.view(&engine, TimeState::Starting);
    assert_eq!(view.holds.iter().map(|h| h.cause).collect::<Vec<_>>(), [Cause::LogRollback]);
    assert!(!view.takes_orders);
}

#[test]
fn a_source_read_over_two_connections_is_named_once_and_down_only_when_both_are() {
    let mut engine = engine();
    quote(&mut engine, T0 + 100, &[("coinbase", "100000"), ("kraken", "100000")]);
    quote(&mut engine, T0 + 4_100, &[("kraken", "100000")]);
    let feeds = FeedStatus::default();
    let sources = |feeds: &FeedStatus| {
        let view = serde_json::to_value(pricing(&engine.markets()[&1], engine.now(), &feeds.venues())).unwrap();
        view["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["source"].as_str().unwrap().to_owned(), s["state"].clone()))
            .collect::<Vec<_>>()
    };
    feeds.publish("coinbase", 0, symbol(SymbolState::Down));
    feeds.publish("coinbase", 1, symbol(SymbolState::Quoting));
    // One connection is down and the other reports nothing wrong: the source is stale, listed once.
    assert_eq!(sources(&feeds), [("coinbase".into(), json!("stale")), ("kraken".into(), json!("counts"))]);
    feeds.publish("coinbase", 1, symbol(SymbolState::Down));
    assert_eq!(sources(&feeds), [("coinbase".into(), json!("down")), ("kraken".into(), json!("counts"))]);
    // A source the engine holds no quote from is listed once too, however many connections carry it.
    let mut fresh = self::engine();
    quote(&mut fresh, T0 + 100, &[("kraken", "100000")]);
    let view = serde_json::to_value(pricing(&fresh.markets()[&1], fresh.now(), &feeds.venues())).unwrap();
    let names: Vec<_> = view["sources"].as_array().unwrap().iter().map(|s| s["source"].as_str().unwrap()).collect();
    assert_eq!(names, ["kraken", "coinbase"]);
}
