use engine_core::price::{MAX_QUOTE_LEAD_MS, SourceQuote};

use super::*;
use tracing::subscriber::set_default;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry;

use crate::alerts::{self, Routes, is_alert_callsite};
use crate::config::{ApiKey, Venue, VenueSymbol};
use crate::feeds::kraken;
use crate::feeds::tests::fake::{FakeEngine, WAIT, coinbase_levels, fast, kraken_book, next_alert};
use crate::feeds::tests::rwa_support::around_the_clock;
use crate::feeds::venue::confirm::Confirmation;
use crate::feeds::venue::quotes::counts;
use crate::feeds::venue::setup::next_backoff;
use crate::metrics::Metrics;
use crate::sequencer::Request;
use crate::test_support::capture_alerts;
use crate::throttle::REALERT_AFTER;

#[tokio::test(start_paused = true)]
async fn refusals_alert_again_after_the_window_with_the_count_between() {
    let (_guard, mut alerts) = capture_alerts();
    let mut feed = kraken_feed(&[(1, "BTC/USD")], WAIT);

    ["first", "second", "third"].into_iter().for_each(|reason| feed.refuse(reason, ""));
    tokio::time::advance(REALERT_AFTER).await;
    feed.refuse("fourth", "");

    let first = next_alert(&mut alerts).await;
    assert!(first["message"].contains("(0 more since the last alert): first"), "{first:?}");
    let again = next_alert(&mut alerts).await;
    assert!(again["message"].contains("(2 more since the last alert): fourth"), "{again:?}");
    assert!(alerts.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn a_vendor_key_echoed_back_never_reaches_an_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let mut feed = kraken_feed(&[(1, "BTC/USD")], WAIT);
    feed.credentials =
        Credentials::ApiKey(ApiKey::from_env("VENDOR_API_KEY", |_| Some("vendor-secret".into())).unwrap());

    feed.refuse("login refused for vendor-secret", r#"{"type":"login_reject","key":"vendor-secret"}"#);
    feed.dropped(&anyhow::anyhow!("authentication with vendor-secret failed"), Duration::from_secs(1));

    for name in ["venue_message_refused", "venue_feed_down"] {
        let alert = next_alert(&mut alerts).await;
        let message = alert["message"].as_str();
        assert_eq!(alert["alert"].as_str(), name);
        assert!(!message.contains("vendor-secret") && message.contains("<redacted>"), "{alert:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn clock_alerts_are_held_to_one_a_window() {
    let (_guard, mut alerts) = capture_alerts();
    let mut feed = kraken_feed(&[(1, "BTC/USD")], WAIT);

    (0..5).for_each(|_| feed.clock_ahead(1, 60_000));
    tokio::time::advance(REALERT_AFTER).await;
    feed.clock_ahead(1, 60_000);

    let first = next_alert(&mut alerts).await;
    assert_eq!(first["alert"].as_str(), "venue_clock_ahead");
    assert!(first["message"].contains("60000ms ahead of the host clock (0 more since the last alert)"), "{first:?}");
    let again = next_alert(&mut alerts).await;
    assert!(again["message"].contains("(4 more since the last alert)"), "{again:?}");
    assert!(alerts.try_recv().is_err());
}

#[test]
fn a_kept_book_for_a_symbol_not_subscribed_is_refused_before_it_is_kept() {
    const NOW: Millis = 1_792_489_875_714;
    // Two-sided books and one-sided ones, which have no top for the subscription check to catch.
    let one_sided_coinbase = r#"{"channel":"l2_data","timestamp":"2026-10-20T09:51:15.9Z","sequence_num":1,"events":[{"type":"snapshot","product_id":"SOL-USD","updates":[{"side":"bid","event_time":"2026-10-20T09:51:15.714274Z","price_level":"100","new_quantity":"1"}]}]}"#;
    let checksum = kraken::checksum([("100", "1.5")].into_iter());
    let one_sided_kraken = format!(
        r#"{{"channel":"book","type":"snapshot","data":[{{"symbol":"SOL/USD","bids":[{{"price":100,"qty":1.5}}],"asks":[],"checksum":{checksum},"timestamp":"2026-10-20T12:34:44.332552Z"}}]}}"#
    );
    let cases = [
        (Venue::Coinbase, ["BTC-USD", "ETH-USD"], coinbase_levels(1, true, "SOL-USD", "100", "101", NOW)),
        (Venue::Coinbase, ["BTC-USD", "ETH-USD"], one_sided_coinbase.to_owned()),
        (Venue::Kraken, ["BTC/USD", "ETH/USD"], kraken_book("SOL/USD", "100", "101", NOW)),
        (Venue::Kraken, ["BTC/USD", "ETH/USD"], one_sided_kraken),
    ];
    for (venue, symbols, message) in cases {
        let conn = VenueConnection {
            venue,
            url: String::new(),
            api_key_env: None,
            api_key_secret: None,
            oauth: None,
            symbols: Vec::new(),
        };
        let refused = Adapter::new(&conn, None, &symbols).read(&message, NOW);
        assert!(
            matches!(&refused, Err(Refusal::Connection(reason)) if reason.contains("SOL") && reason.contains("not subscribed")),
            "{}: {refused:?}",
            venue.name()
        );
    }
}

#[test]
fn the_newest_quote_is_kept_by_the_engines_own_rule() {
    const NOW: Millis = 1_792_489_875_714;
    let at = |ts: Millis| SourceQuote { source: "kraken".into(), mid: Fixed::from_int(100), ts, contract: None };
    let mut pending = Pending::default();
    assert!(!pending.waiting());
    pending.add(at(NOW - 100), NOW).unwrap();
    // One dated before a quote already reached is one the engine would not store.
    pending.add(at(NOW - 300), NOW).unwrap();
    assert_eq!(pending.take().map(|(quote, _)| quote), Some(at(NOW - 100)));
    assert!(!pending.waiting());
    // A quote ahead of the host clock waits for nothing, and a later one dated before it replaces
    // it, as when the venue's clock steps back: the engine would store the first as stamped now.
    pending.add(at(NOW + 400), NOW).unwrap();
    assert!(pending.waiting());
    pending.add(at(NOW + 100), NOW).unwrap();
    assert_eq!(pending.take().map(|(quote, _)| quote), Some(at(NOW + 100)));
    // A quote read ahead of the host clock is kept at the stamp the engine would store it at, the
    // host time it was read, so a move whose stamp is before the venue's but after that replaces it,
    // as when the venue's clock is corrected.
    pending.add(at(NOW + 20), NOW + 10).unwrap();
    let mut moved = at(NOW + 15);
    moved.mid = Fixed::from_int(101);
    pending.add(moved.clone(), NOW + 30).unwrap();
    assert_eq!(pending.take().map(|(quote, _)| quote), Some(moved));
    // The same stamp: the later arrival.
    pending.add(SourceQuote { mid: Fixed::from_int(101), ..at(NOW) }, NOW).unwrap();
    pending.add(at(NOW), NOW).unwrap();
    assert_eq!(pending.take().map(|(quote, _)| quote), Some(at(NOW)));
}

#[test]
fn a_moved_mid_is_kept_when_the_host_clock_steps_back_while_a_repeat_waits() {
    const T: Millis = 1_792_489_875_714;
    let at = |ts: Millis| SourceQuote { source: "kraken".into(), mid: Fixed::from_int(100), ts, contract: None };
    let mut pending = Pending::default();
    // The engine holds a quote stamped T - 100; a repeat of its mid read at T waits, kept at
    // stamp T, the host time it was read.
    pending.add(at(T), T).unwrap();
    // The host clock steps back 80ms, within [time] max_drift, and a moved mid stamped T + 20
    // arrives: it is kept, clamped to the highest host time read, not the stepped-back one.
    let mut moved = at(T + 20);
    moved.mid = Fixed::from_int(101);
    pending.add(moved.clone(), T - 80).unwrap();
    assert_eq!(pending.take(), Some((moved, T)));
    // So is every later update until the clock passes T.
    pending.add(at(T), T).unwrap();
    pending.add(at(T + 30), T - 70).unwrap();
    assert_eq!(pending.take().map(|(quote, _)| quote), Some(at(T + 30)));
}

#[test]
fn a_quote_further_ahead_than_the_engine_takes_is_dropped() {
    const NOW: Millis = 1_792_489_875_714;
    let at = |ts: Millis| SourceQuote { source: "kraken".into(), mid: Fixed::from_int(100), ts, contract: None };
    let mut pending = Pending::default();
    assert_eq!(pending.add(at(NOW + MAX_QUOTE_LEAD_MS), NOW), Ok(()));
    assert_eq!(pending.add(at(NOW + MAX_QUOTE_LEAD_MS + 1), NOW), Err(MAX_QUOTE_LEAD_MS + 1));
    assert_eq!(pending.take().map(|(quote, _)| quote), Some(at(NOW + MAX_QUOTE_LEAD_MS)));
}

#[test]
fn a_quote_counts_within_the_tighter_of_the_feed_and_market_bounds() {
    const AT: Millis = 1_792_489_875_714;
    assert!(counts(AT - 1_000, AT, 1_000, 1_000));
    assert!(!counts(AT - 1_001, AT, 1_000, 1_000));
    assert!(!counts(AT + 1, AT, 1_000, 1_000));
    // The market's rules are tighter than the feed's bound, or looser.
    assert!(counts(AT - 500, AT, 1_000, 500));
    assert!(!counts(AT - 501, AT, 1_000, 500));
    assert!(!counts(AT - 1_001, AT, 1_000, 5_000));
}

#[tokio::test(start_paused = true)]
async fn a_recovery_gives_every_symbol_a_full_silence_window() {
    let (_guard, mut alerts) = capture_alerts();
    let window = Duration::from_millis(300);
    let mut feed = kraken_feed(&[(1, "BTC/USD"), (2, "ETH/USD")], window);
    // Down for longer than the window, so both symbols' counts are past it.
    feed.down = true;
    tokio::time::advance(window * 2).await;

    // BTC quotes again; ETH's count restarts with the recovery, not from before the outage.
    feed.delivered("BTC/USD");
    tokio::time::advance(window - Duration::from_millis(1)).await;
    feed.delivered("BTC/USD");
    assert!(!feed.check_silent());
    assert!(alerts.try_recv().is_err());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(feed.check_silent());
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["symbol"].as_str()), ("venue_symbol_silent", "ETH/USD"));
}

#[test]
fn backoff_doubles_up_to_its_cap() {
    let max = Duration::from_secs(30);
    assert_eq!(next_backoff(Duration::from_millis(500), max), Duration::from_secs(1));
    assert_eq!(next_backoff(Duration::from_secs(20), max), max);
    assert_eq!(next_backoff(max, max), max);
}

#[test]
fn a_connection_that_quoted_retries_at_once_and_one_that_did_not_backs_off() {
    let timing =
        Timing { reconnect_min: Duration::from_millis(500), reconnect_max: Duration::from_secs(30), ..fast(WAIT) };
    let grown = Duration::from_secs(8);
    // A routine drop resets the backoff, whatever it had grown to.
    assert_eq!(next_attempt(grown, true, &timing), (timing.reconnect_min, Duration::from_secs(1)));
    // A failed attempt waits the current backoff and doubles it for the next.
    assert_eq!(next_attempt(grown, false, &timing), (grown, Duration::from_secs(16)));
}

/// Every venue alert keys on its connection, venue, and subject alone, so its repeats share one
/// episode whatever the lead, count, or reason, and each recovery resolves it under the same key.
#[tokio::test(start_paused = true)]
async fn venue_alerts_keep_one_key_per_subject_and_resolve_under_it() {
    crate::test_support::keep_callsites_interested();
    let (layer, router) = alerts::channel(Routes::default(), Arc::new(Metrics::new()));
    let _logging = set_default(registry().with(layer.with_filter(filter_fn(is_alert_callsite))));
    let window = Duration::from_millis(300);
    let mut feed = kraken_feed(&[(1, "BTC/USD"), (2, "ETH/USD")], window);

    feed.dropped(&anyhow::anyhow!("refused"), Duration::from_secs(1));
    feed.delivered("BTC/USD");
    tokio::time::advance(window).await;
    assert!(feed.check_silent());
    feed.delivered("ETH/USD");
    feed.clock_ahead(1, 60_000);
    feed.refuse("unparseable", "{");
    tokio::time::advance(REALERT_AFTER).await;
    feed.clock_ahead(1, 70_000);
    feed.refuse("unknown symbol", "[]");
    feed.confirmed(Confirmation { unlisted: vec!["ETH/USD".into()], ..Confirmation::default() });
    feed.confirmed(Confirmation::default());

    let lines: Vec<(String, bool)> = router.queued().iter().map(|line| (line.key(), line.severity.is_none())).collect();
    let fired = |key: &str| (key.to_owned(), false);
    let resolved = |key: &str| (key.to_owned(), true);
    assert_eq!(
        lines,
        [
            fired("venue_feed_down{feed=wss://ws.kraken.com/v2,venue=kraken}"),
            resolved("venue_feed_down{feed=wss://ws.kraken.com/v2,venue=kraken}"),
            fired("venue_symbol_silent{feed=wss://ws.kraken.com/v2,market=1,symbol=BTC/USD,venue=kraken}"),
            fired("venue_symbol_silent{feed=wss://ws.kraken.com/v2,market=2,symbol=ETH/USD,venue=kraken}"),
            resolved("venue_symbol_silent{feed=wss://ws.kraken.com/v2,market=2,symbol=ETH/USD,venue=kraken}"),
            fired("venue_clock_ahead{feed=wss://ws.kraken.com/v2,market=1,venue=kraken}"),
            fired("venue_message_refused{feed=wss://ws.kraken.com/v2,venue=kraken}"),
            fired("venue_clock_ahead{feed=wss://ws.kraken.com/v2,market=1,venue=kraken}"),
            fired("venue_message_refused{feed=wss://ws.kraken.com/v2,venue=kraken}"),
            fired("venue_market_unlisted{feed=wss://ws.kraken.com/v2,market=2,symbol=ETH/USD,venue=kraken}"),
            resolved("venue_market_unlisted{feed=wss://ws.kraken.com/v2,market=2,symbol=ETH/USD,venue=kraken}"),
        ]
    );
}

/// Two connections of one venue keep their own outage episodes, so one's recovery leaves the
/// other's open.
#[tokio::test(start_paused = true)]
async fn connections_of_one_venue_keep_their_own_alert_episodes() {
    crate::test_support::keep_callsites_interested();
    let (layer, router) = alerts::channel(Routes::default(), Arc::new(Metrics::new()));
    let _logging = set_default(registry().with(layer.with_filter(filter_fn(is_alert_callsite))));
    let mut forex = feed_at(Venue::Massive, "wss://socket.massive.com/forex", &[(10, "EUR-USD")], WAIT);
    let mut stocks = feed_at(Venue::Massive, "wss://socket.massive.com/stocks", &[(40, "AAPL")], WAIT);

    forex.dropped(&anyhow::anyhow!("refused"), Duration::from_secs(1));
    stocks.dropped(&anyhow::anyhow!("refused"), Duration::from_secs(1));
    forex.delivered("EUR-USD");

    let lines: Vec<(String, bool)> = router.queued().iter().map(|line| (line.key(), line.severity.is_none())).collect();
    let key = |path: &str| format!("venue_feed_down{{feed=wss://socket.massive.com/{path},venue=massive}}");
    assert_eq!(lines, [(key("forex"), false), (key("stocks"), false), (key("forex"), true)]);
}

/// A symbol whose contract the listing does not name is alerted and held back until it is named;
/// a silent symbol that the engine stops taking is no longer silent.
#[tokio::test(start_paused = true)]
async fn an_unnamed_contract_alerts_and_a_held_symbol_is_not_silent() {
    crate::test_support::keep_callsites_interested();
    let (layer, router) = alerts::channel(Routes::default(), Arc::new(Metrics::new()));
    let _logging = set_default(registry().with(layer.with_filter(filter_fn(is_alert_callsite))));
    let window = Duration::from_millis(300);
    let mut feed = feed_at(Venue::Kraken, "wss://ws.kraken.com/v2", &[(1, "BTC/USD"), (2, "ETH/USD")], window);

    feed.confirmed(Confirmation { unnamed: vec!["ETH/USD".into()], ..Confirmation::default() });
    assert!(feed.is_held("ETH/USD"));
    tokio::time::advance(window).await;
    assert!(feed.check_silent());
    feed.confirmed(Confirmation { idle: vec!["BTC/USD".into()], ..Confirmation::default() });
    assert!(feed.is_held("BTC/USD") && !feed.is_held("ETH/USD"));

    let lines: Vec<(String, bool)> = router.queued().iter().map(|line| (line.key(), line.severity.is_none())).collect();
    let key = |name: &str, market: u32, symbol: &str| {
        format!("{name}{{feed=wss://ws.kraken.com/v2,market={market},symbol={symbol},venue=kraken}}")
    };
    assert_eq!(
        lines,
        [
            (key("venue_contract_unlisted", 2, "ETH/USD"), false),
            (key("venue_symbol_silent", 1, "BTC/USD"), false),
            (key("venue_contract_unlisted", 2, "ETH/USD"), true),
            (key("venue_symbol_silent", 1, "BTC/USD"), true),
        ]
    );
}

/// A connection that ends with quotes sent while a confirmation is awaited settles with a fresh
/// confirmation, and does not carry the awaited one into the next connection.
#[tokio::test]
async fn settling_a_connection_drops_the_confirmation_it_awaited() {
    let (tx, mut requests) = mpsc::channel(8);
    tokio::spawn(async move {
        let engine = FakeEngine::new();
        while let Some(request) = requests.recv().await {
            if let Request::Query(query) = request {
                engine.answer(query);
            }
        }
    });
    let conn = kraken_feed(&[(1, "BTC/USD")], WAIT).conn;
    let mut feed = VenueFeed::new(conn, Credentials::Public, fast(WAIT), tls_config().unwrap(), tx);
    let (_reply, awaited) = oneshot::channel();
    feed.confirming = Some(awaited);
    feed.unconfirmed.insert("BTC/USD".into());

    feed.settle().await;
    assert!(feed.confirming.is_none() && feed.unconfirmed.is_empty());
}

#[test]
fn a_quote_is_sent_only_while_its_market_stays_open_until_the_sequencer_stamps_it() {
    // Apple shuts for a minute at `shut`; the feed sends every 10ms.
    let mut feed = kraken_feed(&[(40, "AAPL")], WAIT);
    feed.timing.stale_after = Duration::from_secs(3_600);
    let shut = 1_792_504_800_000;
    feed.schedules = Arc::new(BTreeMap::from([(40, around_the_clock(Some((shut, shut + 60_000))))]));
    // Sent 11ms before the close, it is stamped within the interval, while Apple is open.
    assert!(feed.in_session(40, shut - 11, shut - 11));
    // Sent 5ms before the close, it may be stamped after it.
    assert!(!feed.in_session(40, shut - 5, shut - 5));
    // Stamped before the close and sent after the reopen.
    assert!(!feed.in_session(40, shut - 5, shut + 60_000));
    // Stamped ahead of the host clock across the close: the engine checks through the stamp.
    assert!(!feed.in_session(40, shut + 300, shut - 500));
    assert!(feed.in_session(40, shut - 300, shut - 500));
    // A market whose hours the feed has not learnt yet counts as open.
    assert!(feed.in_session(1, shut, shut));
}

#[test]
fn a_cme_read_waits_at_least_two_heartbeat_intervals_and_any_other_the_timeout() {
    let connection = |venue| VenueConnection {
        venue,
        url: String::new(),
        api_key_env: None,
        api_key_secret: None,
        oauth: None,
        symbols: Vec::new(),
    };
    let cme = Adapter::new(&connection(Venue::Cme), None, &["ES:202612"]);
    // CME's heartbeat comes every ten seconds, the template's timeout.
    assert_eq!(cme.read_timeout(Duration::from_secs(10)), Duration::from_secs(20));
    assert_eq!(cme.read_timeout(Duration::from_secs(30)), Duration::from_secs(30));
    let kraken = Adapter::new(&connection(Venue::Kraken), None, &["BTC/USD"]);
    assert_eq!(kraken.read_timeout(Duration::from_secs(10)), Duration::from_secs(10));
}

// ── Helpers ──

/// A Kraken feed for `symbols` that is never run, for calling its alert paths directly.
fn kraken_feed(symbols: &[(MarketId, &str)], timeout: Duration) -> VenueFeed {
    feed_at(Venue::Kraken, "wss://ws.kraken.com/v2", symbols, timeout)
}

/// A `venue` feed on `url` for `symbols` that is never run.
fn feed_at(venue: Venue, url: &str, symbols: &[(MarketId, &str)], timeout: Duration) -> VenueFeed {
    let symbols = symbols.iter().map(|(market, symbol)| VenueSymbol {
        market: *market,
        symbol: (*symbol).into(),
        contract: None,
    });
    let conn = VenueConnection {
        venue,
        url: url.into(),
        api_key_env: None,
        api_key_secret: None,
        oauth: None,
        symbols: symbols.collect(),
    };
    VenueFeed::new(conn, Credentials::Public, fast(timeout), tls_config().unwrap(), mpsc::channel(1).0)
}
