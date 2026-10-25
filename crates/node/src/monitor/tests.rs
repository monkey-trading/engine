use super::*;

use std::path::Path;

use chrono::DateTime;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, Sequenced};
use engine_core::event::Event;
use engine_core::futures::FuturesContract;
use engine_core::market::MarketSpec;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};
use tracing::subscriber::set_default;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry;

use crate::NodeConfig;
use crate::alerts::{Routes, Severity, capture, channel, is_alert_callsite};
use crate::feeds::configured_sources;
use crate::sequencer::genesis_commands;

#[test]
fn a_sample_reads_house_markets_and_each_venues_newest_quote() {
    let mut engine = dev_engine();
    sequence(&mut engine, T0, prices(BTC, &[("venue-a", T0 - 100), ("venue-b", T0 - 50)]));
    // An older quote from venue-a does not replace its newer one.
    sequence(&mut engine, T0 + 10, prices(BTC, &[("venue-a", T0 - 500)]));
    let sample = Sample::of(&engine);
    assert_eq!(sample.house_nav, Fixed::from_int(10_000_000));
    let btc = &sample.markets.iter().find(|m| m.id == BTC).unwrap();
    assert!(btc.open);
    assert_eq!(btc.price, Ok(()));
    assert_eq!(btc.quotes, [("venue-a".to_owned(), T0 - 100), ("venue-b".to_owned(), T0 - 50)]);
    let eth = &sample.markets.iter().find(|m| m.id == ETH).unwrap();
    assert_eq!(eth.price, Err(Unpriced::Index(PriceError::Stale { fresh: 0, needed: 2 })));

    let metrics = Metrics::new();
    let fed = BTreeMap::from([(BTC, BTreeSet::from(["venue-a".to_owned(), "venue-b".to_owned()]))]);
    sample.record(&metrics, &fed, T0);
    let text = metrics.encode();
    assert!(text.contains("engine_house_nav 10000000.0\n"), "{text}");
    assert!(text.contains("engine_market_priced{market=\"1\"} 1\n"), "{text}");
    assert!(text.contains("engine_market_priced{market=\"2\"} 0\n"), "{text}");
    assert!(text.contains("engine_market_open{market=\"1\"} 1\n"), "{text}");
    // Every market's fill counter exists before its first fill.
    assert!(text.contains("engine_fills_total{market=\"2\"} 0\n"), "{text}");
    let quote_ts = (T0 - 50) as f64 / 1000.0;
    let quote = format!("engine_quote_timestamp_seconds{{market=\"1\",venue=\"venue-b\"}} {quote_ts:?}\n");
    assert!(text.contains(&quote), "{text}");
}

#[test]
fn a_client_long_shows_as_house_short_net_and_positive_gross_exposure() {
    let mut engine = dev_engine();
    sequence(&mut engine, T0, prices(BTC, &[("venue-a", T0), ("venue-b", T0)]));
    let deposit = Command::Deposit { account: ACCOUNT, amount: Fixed::from_int(100_000), chain_ref: "dev:1".into() };
    sequence(&mut engine, T0, deposit);
    sequence(&mut engine, T0, buy_btc("0.5"));
    let metrics = Metrics::new();
    Sample::of(&engine).record(&metrics, &BTreeMap::new(), T0);
    let text = metrics.encode();
    assert!(text.contains("engine_house_net_exposure{market=\"1\"} -50000.0\n"), "{text}");
    assert!(text.contains("engine_house_net_exposure{market=\"2\"} 0.0\n"), "{text}");
    assert!(text.contains("engine_house_gross_exposure 50000.0\n"), "{text}");
}

/// Net exposure is in USD, as NAV and gross exposure are, so a market quoted in euros converts at
/// its FX mark.
#[test]
fn net_exposure_in_a_market_quoted_in_another_currency_is_in_usd() {
    let mut engine = dev_engine();
    sequence(&mut engine, T0, quoted(EUR_USD, "1.08"));
    sequence(&mut engine, T0, quoted(DE40, "23500"));
    let deposit = Command::Deposit { account: ACCOUNT, amount: Fixed::from_int(100_000), chain_ref: "dev:1".into() };
    sequence(&mut engine, T0, deposit);
    let filled = sequence(&mut engine, T0, buy(DE40, "1", Fixed::from_int(23_500)));
    assert!(filled.iter().any(|e| matches!(e, Event::Fill(_))), "{filled:?}");
    let metrics = Metrics::new();
    Sample::of(&engine).record(&metrics, &BTreeMap::new(), T0);
    let text = metrics.encode();
    assert!(text.contains("engine_house_net_exposure{market=\"23\"} -25380.0\n"), "{text}");
    assert!(text.contains("engine_house_gross_exposure 25380.0\n"), "{text}");
}

/// A fill in a market quoted in another currency needs its FX index too, so the market counts as
/// priced, and its no-price alert resolves, only while both are usable.
#[test]
fn a_market_quoted_in_another_currency_is_priced_only_while_its_fx_market_is() {
    let mut engine = dev_engine();
    sequence(&mut engine, T0, quoted(EUR_USD, "1.08"));
    sequence(&mut engine, T0, quoted(DE40, "23500"));
    assert_eq!(market(&engine, DE40).price, Ok(()));
    // EUR-USD's quotes age past its rules while DE40 keeps quoting.
    let later = T0 + 1_001;
    sequence(&mut engine, later, prices_at(DE40, "23500", later));
    let fx_stale = Err(Unpriced::Fx(Reject::NoFxPrice { fx_market: EUR_USD }));
    assert_eq!(market(&engine, DE40).price, fx_stale);
    let after = Duration::from_secs(5);
    let t0 = Instant::now();
    let mut no_price = NoPrice::new(BTreeSet::from([DE40, EUR_USD]));
    let lines = capture(|| {
        no_price.check(&Sample::of(&engine), t0, after);
        no_price.check(&Sample::of(&engine), t0 + after, after);
    });
    let summaries: Vec<_> = lines.iter().map(|line| (line.key(), line.summary.clone())).collect();
    let de40 = (
        "market_no_price{market=23}".to_owned(),
        "DE40 has had no usable price for 5s: its FX market 10 has no usable price".to_owned(),
    );
    assert!(summaries.contains(&de40), "{summaries:?}");
    // Once EUR-USD quotes again, DE40 is priced and resolves.
    sequence(&mut engine, later, prices_at(EUR_USD, "1.08", later));
    assert_eq!(market(&engine, DE40).price, Ok(()));
    let lines = capture(|| no_price.check(&Sample::of(&engine), t0 + after * 2, after));
    assert!(lines.iter().any(|line| line.key() == de40.0 && line.severity.is_none()), "{lines:?}");
}

#[tokio::test]
async fn queue_depth_counts_the_requests_waiting_for_the_sequencer() {
    let (tx, _rx) = mpsc::channel(8);
    assert_eq!(queue_depth(&tx), 0);
    for _ in 0..3 {
        tx.send(Request::Submit { cmd: Command::Tick, reply: None }).await.unwrap();
    }
    assert_eq!(queue_depth(&tx), 3);
}

#[test]
fn a_stalled_sequencer_alerts_once_after_the_threshold_and_resolves_when_it_answers() {
    let after = Duration::from_secs(5);
    let t0 = Instant::now();
    let at = |secs| t0 + Duration::from_secs(secs);
    let mut stall = Stall::default();
    let mut check = |answered, secs| capture(|| stall.check(answered, at(secs), after));
    assert!(check(true, 0).is_empty());
    assert!(check(false, 1).is_empty());
    assert!(check(false, 5).is_empty());
    let [alert] = check(false, 6).try_into().unwrap();
    assert_eq!((alert.key(), alert.severity), ("sequencer_stalled{}".to_owned(), Some(Severity::Critical)));
    assert!(check(false, 30).is_empty());
    let [resolve] = check(true, 31).try_into().unwrap();
    assert_eq!((resolve.key(), resolve.severity), (alert.key(), None));
    assert!(check(true, 32).is_empty());
}

#[test]
fn a_futures_market_on_its_last_listed_contract_warns_once_until_a_next_is_listed() {
    let mut last = LastContract::default();
    let on = |contract: Option<&str>| {
        let market = MarketSample { last_contract: contract.map(Into::into), ..market_sample(20, true, Ok(())) };
        sample(vec![market])
    };
    let mut check = |sample: Sample| capture(|| last.check(&sample));
    assert!(check(on(None)).is_empty());
    let [alert] = check(on(Some("ESH8"))).try_into().unwrap();
    assert_eq!((alert.key(), alert.severity), ("futures_last_contract{market=20}".to_owned(), Some(Severity::Warning)));
    assert!(check(on(Some("ESH8"))).is_empty());
    let [resolve] = check(on(None)).try_into().unwrap();
    assert_eq!((resolve.key(), resolve.severity), (alert.key(), None));
}

#[test]
fn a_sample_names_a_futures_markets_contract_only_while_it_is_the_last_listed() {
    let mut cfg = dev_config();
    let us500 = cfg.genesis.markets.iter().find(|m| m.id == 20).expect("US 500").clone();
    let es = |name: &str, from: Millis| FuturesContract {
        contract: name.into(),
        from: DateTime::from_timestamp_millis(from).expect("in range"),
    };
    let day = 86_400_000;
    let last = es("ESZ6", T0 - day);
    let with_next = vec![last.clone(), es("ESH7", T0 + day)];
    for (id, contracts) in [(70, vec![last]), (71, with_next)] {
        cfg.genesis.markets.push(MarketSpec { id, symbol: format!("ES{id}"), contracts, ..us500.clone() });
    }
    let engine = engine_of(&cfg);
    assert_eq!(market(&engine, 70).last_contract.as_deref(), Some("ESZ6"));
    assert_eq!(market(&engine, 71).last_contract, None);
    assert_eq!(market(&engine, 20).last_contract, None);
}

/// The engine keeps every source it has heard from, and none it has not, so quote times come from
/// the config's sources: a venue taken out of the config would age forever and hide a live one going
/// stale, and one that never quoted would be missing. Every fed venue has a row from the first
/// sample, at the node's start until it quotes, and no retired one has a row, for mock and venue
/// feeds alike.
#[test]
fn every_fed_venue_has_a_quote_time_from_the_start_and_no_retired_one_has_any() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/dev.toml");
    let dev = std::fs::read_to_string(path).unwrap();
    let venues = dev.replace(r#"provider = "mock" # "venues""#, r#"provider = "venues" # not "mock""#);
    assert_ne!(venues, dev, "the replacement must change the config");
    let started = T0 - 60_000;
    let row = |market: MarketId, venue: &str, ts: Millis| {
        format!("engine_quote_timestamp_seconds{{market=\"{market}\",venue=\"{venue}\"}} {:?}\n", ts as f64 / 1000.0)
    };
    // (config, the sources it feeds BTC from, a source the engine still holds but the config dropped)
    for (text, fed, retired) in
        [(&dev, ["mock-a", "mock-b", "mock-c"], "kraken"), (&venues, ["bitstamp", "coinbase", "kraken"], "mock-a")]
    {
        let cfg: NodeConfig = toml::from_str(text).unwrap();
        let sources = configured_sources(&cfg.feeds);
        assert_eq!(sources[&BTC], fed.map(str::to_owned).into(), "{sources:?}");

        // Before genesis lists a market, every pair the config feeds reads as stale from the start.
        let metrics = Metrics::new();
        Sample::of(&Engine::new()).record(&metrics, &sources, started);
        let text = metrics.encode();
        for (market, venue) in sources.iter().flat_map(|(market, venues)| venues.iter().map(|venue| (*market, venue))) {
            assert!(text.contains(&row(market, venue, started)), "{text}");
        }

        // The last fed venue never quotes BTC; the retired one quoted a day ago.
        let [quoting @ .., silent] = fed;
        let mut engine = dev_engine();
        let quotes: Vec<_> = quoting.iter().map(|venue| (*venue, T0)).chain([(retired, T0 - 86_400_000)]).collect();
        sequence(&mut engine, T0, prices(BTC, &quotes));
        let metrics = Metrics::new();
        Sample::of(&engine).record(&metrics, &sources, started);
        let text = metrics.encode();
        for venue in quoting {
            assert!(text.contains(&row(BTC, venue, T0)), "{text}");
        }
        assert!(text.contains(&row(BTC, silent, started)), "{text}");
        assert!(!text.contains(&format!("venue=\"{retired}\"")), "{text}");
    }
}

#[test]
fn a_market_goes_unpriced_once_its_quotes_age_past_the_rules() {
    let mut engine = dev_engine();
    sequence(&mut engine, T0, prices(BTC, &[("venue-a", T0), ("venue-b", T0)]));
    // BTC's quotes count for 1s.
    sequence(&mut engine, T0 + 1_000, Command::Tick);
    assert_eq!(market(&engine, BTC).price, Ok(()));
    sequence(&mut engine, T0 + 1_001, Command::Tick);
    assert_eq!(market(&engine, BTC).price, Err(Unpriced::Index(PriceError::Stale { fresh: 0, needed: 2 })));
}

#[test]
fn no_price_alerts_once_after_the_threshold_and_resolves_when_priced() {
    let after = Duration::from_secs(5);
    let t0 = Instant::now();
    let at = |secs| t0 + Duration::from_secs(secs);
    let unpriced = || sample(vec![market_sample(1, true, Err(PriceError::Stale { fresh: 0, needed: 2 }))]);
    let priced = || sample(vec![market_sample(1, true, Ok(()))]);
    let mut no_price = NoPrice::new(BTreeSet::from([1]));
    let mut check = |sample: Sample, secs| capture(|| no_price.check(&sample, at(secs), after));
    assert!(check(unpriced(), 0).is_empty());
    assert!(check(unpriced(), 4).is_empty());
    let [alert] = check(unpriced(), 5).try_into().unwrap();
    assert_eq!((alert.name.as_str(), alert.severity), ("market_no_price", Some(Severity::Critical)));
    assert_eq!(alert.key(), "market_no_price{market=1}");
    assert_eq!(alert.summary, "M1 has had no usable price for 5s: only 0 fresh sources, need 2");
    // Still unpriced: the episode is open, so nothing more.
    assert!(check(unpriced(), 30).is_empty());
    let [resolve] = check(priced(), 31).try_into().unwrap();
    assert_eq!((resolve.severity, resolve.key()), (None, alert.key()));
    assert!(check(priced(), 32).is_empty());
}

/// Nothing on the node can price a market no configured feed prices, so each is named once in its
/// own `market_unfed` Warning and never pages; a market a feed prices still pages when it loses its
/// price.
#[test]
fn a_market_no_configured_feed_prices_is_named_once_and_never_pages() {
    let after = Duration::from_secs(5);
    let t0 = Instant::now();
    let at = |secs| t0 + Duration::from_secs(secs);
    let stale = || Err(PriceError::Stale { fresh: 0, needed: 2 });
    let markets =
        |ids: &[MarketId], open: bool| sample(ids.iter().map(|id| market_sample(*id, open, stale())).collect());
    // A feed prices market 1 only; markets 2 and 3 have none.
    let mut no_price = NoPrice::new(BTreeSet::from([1]));
    let mut check = |sample: Sample, secs| capture(|| no_price.check(&sample, at(secs), after));

    // Each unfed market raises its own Warning, keyed by the market, so one listing is not
    // swallowed by another's quiet window.
    let [m2, m3] = check(markets(&[1, 2, 3], true), 0).try_into().unwrap();
    for (line, market) in [(m2, 2), (m3, 3)] {
        assert_eq!((line.key(), line.severity), (format!("market_unfed{{market={market}}}"), Some(Severity::Warning)));
        assert_eq!(
            line.summary,
            format!("no configured feed prices M{market}: it cannot trade, and does not page market_no_price")
        );
    }
    let [alert] = check(markets(&[1, 2, 3], true), 5).try_into().unwrap();
    assert_eq!((alert.key(), alert.severity), ("market_no_price{market=1}".to_owned(), Some(Severity::Critical)));
    // A session close and reopen pages nothing for the unfed markets.
    assert!(check(markets(&[1, 2, 3], true), 60).is_empty());
    let [resolve] = check(markets(&[1, 2, 3], false), 61).try_into().unwrap();
    assert_eq!((resolve.key(), resolve.severity), (alert.key(), None));
    assert!(check(markets(&[1, 2, 3], true), 62).is_empty());
    let [again] = check(markets(&[1, 2, 3], true), 67).try_into().unwrap();
    assert_eq!(again.key(), alert.key());
    assert!(check(markets(&[1, 2, 3], true), 120).is_empty());

    // Markets listed later with no feed are each named once, in their own episode.
    let [listed] = check(markets(&[1, 2, 3, 4], true), 121).try_into().unwrap();
    assert_eq!((listed.key(), listed.severity), ("market_unfed{market=4}".to_owned(), Some(Severity::Warning)));
    assert!(check(markets(&[1, 2, 3, 4], true), 180).is_empty());
    let [second] = check(markets(&[1, 2, 3, 4, 5], true), 181).try_into().unwrap();
    assert_eq!((second.key(), second.severity), ("market_unfed{market=5}".to_owned(), Some(Severity::Warning)));
    assert!(check(markets(&[1, 2, 3, 4, 5], true), 240).is_empty());
}

#[test]
fn a_closed_or_briefly_unpriced_market_raises_nothing() {
    let after = Duration::from_secs(5);
    let t0 = Instant::now();
    let at = |secs| t0 + Duration::from_secs(secs);
    let stale = || Err(PriceError::Stale { fresh: 0, needed: 2 });
    let mut no_price = NoPrice::new(BTreeSet::from([1]));
    let lines = capture(|| {
        no_price.check(&sample(vec![market_sample(1, false, stale())]), at(0), after);
        no_price.check(&sample(vec![market_sample(1, false, stale())]), at(60), after);
        // Unpriced for 4s, priced, then unpriced again: the clock restarts.
        no_price.check(&sample(vec![market_sample(1, true, stale())]), at(61), after);
        no_price.check(&sample(vec![market_sample(1, true, Ok(()))]), at(65), after);
        no_price.check(&sample(vec![market_sample(1, true, stale())]), at(66), after);
        no_price.check(&sample(vec![market_sample(1, true, stale())]), at(70), after);
    });
    assert!(lines.is_empty(), "{lines:?}");
}

#[tokio::test(start_paused = true)]
async fn the_monitor_pages_a_stalled_sequencer_resolves_it_and_stops_when_the_sequencer_goes() {
    crate::test_support::keep_callsites_interested();
    let metrics = Arc::new(Metrics::new());
    let (layer, router) = channel(Routes::default(), metrics.clone());
    let _logging = set_default(registry().with(layer.with_filter(filter_fn(is_alert_callsite))));
    let (tx, mut rx) = mpsc::channel(8);
    let monitor = tokio::spawn(run(tx, metrics, Duration::from_secs(3), BTreeMap::new()));
    // A stalled sequencer takes queries but answers none; dropping one would mean it is gone.
    let mut unanswered = Vec::new();
    for _ in 0..6 {
        let Some(Request::Query(query)) = rx.recv().await else { panic!("expected a sample query") };
        unanswered.push(query);
    }
    let Some(Request::Query(query)) = rx.recv().await else { panic!("expected a sample query") };
    query(&Engine::new());
    // Wait for the next query, so the answered sample has been handled.
    let Some(Request::Query(_)) = rx.recv().await else { panic!("expected a sample query") };
    drop(rx);
    let stopped = monitor.await.unwrap();
    assert!(format!("{:#}", stopped.unwrap_err()).contains("sequencer gone"));

    let lines = router.queued();
    let [stalled, resumed] = lines.as_slice() else { panic!("{lines:?}") };
    assert_eq!((stalled.key(), stalled.severity), ("sequencer_stalled{}".to_owned(), Some(Severity::Critical)));
    assert_eq!((resumed.key(), resumed.severity), (stalled.key(), None));
}

// ── Helpers ──

const BTC: MarketId = 1;
const ETH: MarketId = 2;
const EUR_USD: MarketId = 10;
/// Quoted in euros, converting at EUR-USD.
const DE40: MarketId = 23;
const ACCOUNT: AccountId = AccountId([0xaa; 20]);
/// Tue 20 Oct 2026 14:00 UTC.
const T0: Millis = 1_792_504_800_000;

/// Historical V19 market/metrics fixtures, stamped at `T0`.
fn dev_engine() -> Engine {
    engine_of(&dev_config())
}

fn dev_config() -> NodeConfig {
    NodeConfig::load(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/dev.toml"))).unwrap()
}

/// An engine after `cfg`'s protocol activation and genesis, as a node sequences them, at `T0`.
fn engine_of(cfg: &NodeConfig) -> Engine {
    let activation = [
        AdminCommand::SetClientAuth(cfg.client_auth().unwrap()),
        AdminCommand::SetOperator(cfg.operator().unwrap()),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::V19 },
    ];
    let mut engine = Engine::new();
    for cmd in activation.into_iter().map(Command::Admin).chain(genesis_commands(&cfg.genesis)) {
        sequence(&mut engine, T0, cmd);
    }
    engine
}

fn sequence(engine: &mut Engine, ts: Millis, cmd: Command) -> Vec<Event> {
    let seq = engine.last_seq().map_or(0, |s| s + 1);
    engine.apply(&Sequenced { seq, ts, cmd }).unwrap()
}

fn prices(market: MarketId, quotes: &[(&str, Millis)]) -> Command {
    let quotes = quotes
        .iter()
        .map(|(source, ts)| SourceQuote {
            source: (*source).to_owned(),
            mid: Fixed::from_int(100_000),
            ts: *ts,
            contract: None,
        })
        .collect();
    Command::Prices { market, quotes }
}

/// Quotes at `mid` from two sources, stamped `ts`.
fn prices_at(market: MarketId, mid: &str, ts: Millis) -> Command {
    let quote = |source: &str| SourceQuote { source: source.to_owned(), mid: mid.parse().unwrap(), ts, contract: None };
    Command::Prices { market, quotes: vec![quote("venue-a"), quote("venue-b")] }
}

/// `prices_at` stamped `T0`.
fn quoted(market: MarketId, mid: &str) -> Command {
    prices_at(market, mid, T0)
}

/// A market buy of `qty` BTC at the test mid.
fn buy_btc(qty: &str) -> Command {
    buy(BTC, qty, Fixed::from_int(100_000))
}

/// A market buy of `qty` in `market`, quoted at `quoted`.
fn buy(market: MarketId, qty: &str, quoted: Fixed) -> Command {
    Command::PlaceOrder(OrderRequest {
        account: ACCOUNT,
        client_id: 1,
        market,
        side: Side::Buy,
        qty: qty.parse().unwrap(),
        kind: OrderKind::Market { quoted, max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    })
}

fn market(engine: &Engine, id: MarketId) -> MarketSample {
    Sample::of(engine).markets.into_iter().find(|m| m.id == id).unwrap()
}

fn market_sample(id: MarketId, open: bool, price: Result<(), PriceError>) -> MarketSample {
    let price = price.map_err(Unpriced::Index);
    MarketSample {
        id,
        symbol: format!("M{id}"),
        open,
        price,
        house_net_exposure: Fixed::ZERO,
        quotes: Vec::new(),
        last_contract: None,
    }
}

fn sample(markets: Vec<MarketSample>) -> Sample {
    Sample { house_nav: Fixed::ZERO, house_gross_exposure: Fixed::ZERO, markets }
}
