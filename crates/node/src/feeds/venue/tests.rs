use engine_core::price::{MAX_QUOTE_LEAD_MS, SourceQuote};

use super::*;
use tracing::subscriber::set_default;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry;

use crate::alerts::{self, Routes, is_alert_callsite};
use crate::config::{ApiKey, VenueSymbol};
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
    feed.api_key = Some(ApiKey::from_env("VENDOR_API_KEY", |_| Some("vendor-secret".into())).unwrap());

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
fn the_newest_quote_the_host_clock_has_reached_is_taken_and_those_ahead_wait() {
    const NOW: Millis = 1_792_489_875_714;
    let at = |ts: Millis| SourceQuote { source: "kraken".into(), mid: Fixed::from_int(100), ts, contract: None };
    let mut pending = Pending::default();
    // Received over one tick from a venue whose clock is a little ahead of the host's.
    for ts in [NOW - 300, NOW - 100, NOW + 40, NOW + 400] {
        assert_eq!(pending.add(at(ts), NOW), Ok(()));
    }
    assert_eq!(pending.take(NOW), Some(at(NOW - 100)));
    assert_eq!(pending.take(NOW), None);
    // Each quote ahead goes once the host clock reaches it.
    assert_eq!(pending.take(NOW + 40), Some(at(NOW + 40)));
    assert_eq!(pending.take(NOW + 1_000), Some(at(NOW + 400)));
    assert_eq!(pending.take(NOW + 1_000), None);
    // A later quote dated before those still ahead, as after the venue's clock steps back,
    // supersedes them: they never go.
    pending.add(at(NOW + 800), NOW).unwrap();
    pending.add(at(NOW + 500), NOW).unwrap();
    pending.add(at(NOW + 100), NOW + 100).unwrap();
    assert_eq!(pending.take(NOW + 200), Some(at(NOW + 100)));
    assert_eq!(pending.take(NOW + 1_000), None);
    // Of a quote that waited and one received after its time, the newer goes; on a tie, the later.
    pending.add(at(NOW + 50), NOW).unwrap();
    pending.add(SourceQuote { mid: Fixed::from_int(101), ..at(NOW + 50) }, NOW + 60).unwrap();
    assert_eq!(pending.take(NOW + 60).map(|quote| quote.mid), Some(Fixed::from_int(101)));
    pending.add(at(NOW + 70), NOW).unwrap();
    pending.add(at(NOW + 80), NOW + 90).unwrap();
    assert_eq!(pending.take(NOW + 90), Some(at(NOW + 80)));
}

#[test]
fn the_engine_holds_what_it_would_if_every_quote_went_once_its_time_came() {
    // The engine stores a quote in place of the one it holds unless that one is newer and its time
    // already reached.
    let stores = |held: &Option<SourceQuote>, quote: &SourceQuote, now: Millis| {
        held.as_ref().is_none_or(|held| quote.ts >= held.ts || held.ts > now)
    };
    let quote =
        |id: i64, ts: Millis| SourceQuote { source: "kraken".into(), mid: Fixed::from_int(id), ts, contract: None };
    // Every way three quotes can arrive at 0, 10, 20 or 30 ms dated 0 to 40 ms, with the engine
    // first holding nothing, a reached quote, or one still ahead, and a tick every 10 ms from 5 ms.
    // The reference sends, at each tick, every quote whose time has come, in arrival order, and
    // withdraws a quote still ahead of the host clock when a later one is dated before it, as when
    // the venue's clock steps back. After every tick the engine holds the same quote either way.
    let times: Vec<(Millis, Millis)> = (0..4).flat_map(|at| (0..5).map(move |ts| (at * 10, ts * 10))).collect();
    let grid = times.as_slice();
    for start in [None, Some(quote(0, -10)), Some(quote(0, 50))] {
        for (first, second, third) in
            grid.iter().flat_map(move |a| grid.iter().flat_map(move |b| grid.iter().map(move |c| (a, b, c))))
        {
            let mut arrivals =
                [(first.0, quote(1, first.1)), (second.0, quote(2, second.1)), (third.0, quote(3, third.1))];
            // Stable, so quotes arriving together keep their order.
            arrivals.sort_by_key(|(at, _)| *at);
            let case = format!("{start:?} then {arrivals:?}");
            let (mut reference, mut held, mut pending) = (start.clone(), start.clone(), Pending::default());
            let mut waiting: Vec<SourceQuote> = Vec::new();
            let mut arrivals = arrivals.into_iter().peekable();
            for tick in (5..=55).step_by(10) {
                while let Some((at, arrived)) = arrivals.next_if(|(at, _)| *at < tick) {
                    waiting.retain(|quote| quote.ts <= at || quote.ts <= arrived.ts);
                    waiting.push(arrived.clone());
                    pending.add(arrived, at).unwrap();
                }
                let (due, ahead): (Vec<_>, Vec<_>) = waiting.into_iter().partition(|quote| quote.ts <= tick);
                waiting = ahead;
                for quote in due {
                    if stores(&reference, &quote, tick) {
                        reference = Some(quote);
                    }
                }
                if let Some(taken) = pending.take(tick)
                    && stores(&held, &taken, tick)
                {
                    held = Some(taken);
                }
                assert_eq!(held, reference, "at {tick}: {case}");
            }
        }
    }
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
        let conn = VenueConnection { venue, url: String::new(), api_key_env: None, dataset: None, symbols: Vec::new() };
        let refused = Adapter::new(&conn, None, &symbols).read(&message, NOW);
        assert!(
            matches!(&refused, Err(Refusal::Connection(reason)) if reason.contains("SOL") && reason.contains("not subscribed")),
            "{}: {refused:?}",
            venue.name()
        );
    }
}

#[test]
fn a_quote_further_ahead_than_the_engine_takes_is_dropped() {
    const NOW: Millis = 1_792_489_875_714;
    let at = |ts: Millis| SourceQuote { source: "kraken".into(), mid: Fixed::from_int(100), ts, contract: None };
    let mut pending = Pending::default();
    assert_eq!(pending.add(at(NOW + MAX_QUOTE_LEAD_MS), NOW), Ok(()));
    assert_eq!(pending.add(at(NOW + MAX_QUOTE_LEAD_MS + 1), NOW), Err(MAX_QUOTE_LEAD_MS + 1));
    assert_eq!(pending.take(NOW + 60_000), Some(at(NOW + MAX_QUOTE_LEAD_MS)));
}

#[test]
fn pending_reports_what_waits_and_when_the_clock_reaches_it() {
    const NOW: Millis = 1_792_489_875_714;
    let at = |ts: Millis| SourceQuote { source: "kraken".into(), mid: Fixed::from_int(100), ts, contract: None };
    let mut pending = Pending::default();
    assert!(!pending.reached(NOW));
    assert_eq!(pending.next_ahead(NOW), None);
    pending.add(at(NOW - 100), NOW).unwrap();
    pending.add(at(NOW + 40), NOW).unwrap();
    assert!(pending.reached(NOW));
    assert_eq!(pending.next_ahead(NOW), Some(NOW + 40));
    // Once the clock reaches it, the waiting quote is the reached one, and nothing stays ahead.
    assert!(pending.reached(NOW + 40));
    assert_eq!(pending.next_ahead(NOW + 40), None);
    assert_eq!(pending.take(NOW + 40), Some(at(NOW + 40)));
    assert!(!pending.reached(NOW + 40));
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
    let mut feed = VenueFeed::new(conn, None, fast(WAIT), tls_config().unwrap(), tx);
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
    // A market whose hours the feed has not learnt yet counts as open.
    assert!(feed.in_session(1, shut, shut));
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
    let conn = VenueConnection { venue, url: url.into(), api_key_env: None, dataset: None, symbols: symbols.collect() };
    VenueFeed::new(conn, None, fast(timeout), tls_config().unwrap(), mpsc::channel(1).0)
}
