use std::sync::Arc;

use tokio::sync::mpsc;

use super::*;
use crate::config::{VenueConnection, VenueSymbol};
use crate::feeds::tests::fake::{WAIT, fast};
use crate::feeds::tests::rwa_support::around_the_clock;
use crate::feeds::venue::setup::tls_config;

const MS: Duration = Duration::from_millis(1);
/// A host clock far past every stamp below, so it caps none.
const LATE: Millis = 1_000_000;

#[test]
fn a_book_read_before_an_answered_ping_is_stamped_at_the_ping() {
    let at = Instant::now();
    let mut held = StandingBooks::default();
    held.hold_at("BTC", quote("100", 1_000), at, at, 1);
    // The ping went out 300 ms after the read, so the venue showed the book 300 ms past its stamp.
    let redated = held.redate(ping(1, at + MS * 300, LATE));
    assert_eq!(redated, [Redated { symbol: "BTC".into(), read_ts: 1_000, quote: quote("100", 1_300) }]);
}

#[test]
fn a_book_read_after_the_ping_went_out_is_not_redated_by_its_answer() {
    let at = Instant::now();
    let mut held = StandingBooks::default();
    // Read after ping 1 went out and before ping 2.
    held.hold_at("BTC", quote("100", 1_000), at + MS * 100, at, 2);
    assert_eq!(held.redate(ping(1, at, LATE)), []);
    // A pong answering ping 2 or a later one vouches for it.
    let stamps = |number| held.redate(ping(number, at + MS * 400, LATE)).iter().map(|r| r.quote.ts).collect::<Vec<_>>();
    assert_eq!(stamps(2), [1_300]);
    assert_eq!(stamps(3), [1_300]);
}

#[test]
fn a_redated_stamp_never_passes_the_host_clock_when_the_ping_went_out() {
    let at = Instant::now();
    let mut held = StandingBooks::default();
    held.hold_at("BTC", quote("100", 1_000), at, at, 0);
    let stamps = |sent, sent_ms| held.redate(ping(0, sent, sent_ms)).iter().map(|r| r.quote.ts).collect::<Vec<_>>();
    assert_eq!(stamps(at + MS * 300, 1_100), [1_100]);
    // A host clock at or behind the book's stamp when the ping went out, or a ping sent as the book
    // was read, adds nothing.
    assert_eq!(stamps(at + MS * 300, 1_000), Vec::<Millis>::new());
    assert_eq!(stamps(at + MS * 300, 900), Vec::<Millis>::new());
    assert_eq!(stamps(at, LATE), Vec::<Millis>::new());
}

#[test]
fn the_window_runs_from_the_ping_answered_before_the_book_was_read() {
    let at = Instant::now();
    let mut held = StandingBooks::default();
    // BTC was read the moment its vouching ping was answered; ETH 400 ms after, as when the host
    // delays a book's read, which buys it no time.
    held.hold_at("BTC", quote("100", 1_000), at, at, 0);
    held.hold_at("ETH", quote("10", 1_000), at + MS * 400, at, 0);
    let edge = at + MAX_BOOK_SILENCE;
    assert_eq!(held.expire(edge), Vec::<String>::new());
    let silence = Millis::try_from(MAX_BOOK_SILENCE.as_millis()).expect("in range");
    let stamps = |held: &StandingBooks, sent| {
        held.redate(ping(0, sent, LATE)).into_iter().map(|r| (r.symbol, r.quote.ts)).collect::<Vec<_>>()
    };
    // Each stamp counts from its own read.
    assert_eq!(stamps(&held, edge), [("BTC".into(), 1_000 + silence), ("ETH".into(), 1_000 + silence - 400)]);
    // A millisecond on, both windows have run out.
    assert_eq!(held.expire(edge + MS), ["BTC", "ETH"]);
    assert_eq!(stamps(&held, edge + MS), []);
}

#[test]
fn a_forgotten_or_cleared_book_is_not_redated() {
    let at = Instant::now();
    let mut held = StandingBooks::default();
    held.hold_at("BTC", quote("100", 1_000), at, at, 0);
    held.hold_at("ETH", quote("10", 1_000), at, at, 0);
    held.forget("BTC");
    let symbols = |held: &StandingBooks| {
        held.redate(ping(0, at + MS * 100, LATE)).into_iter().map(|r| r.symbol).collect::<Vec<_>>()
    };
    assert_eq!(symbols(&held), ["ETH"]);
    held.clear();
    assert_eq!(symbols(&held), Vec::<String>::new());
}

#[test]
fn a_newer_book_replaces_the_one_held() {
    let at = Instant::now();
    let mut held = StandingBooks::default();
    held.hold_at("BTC", quote("100", 1_000), at, at, 0);
    held.hold_at("BTC", quote("101", 1_200), at + MS * 200, at, 0);
    let redated = held.redate(ping(0, at + MS * 300, LATE));
    assert_eq!(redated, [Redated { symbol: "BTC".into(), read_ts: 1_200, quote: quote("101", 1_300) }]);
}

#[test]
fn a_stamp_no_later_than_one_given_the_symbol_is_left_out() {
    let at = Instant::now();
    let mut held = StandingBooks::default();
    held.hold_at("BTC", quote("100", 1_000), at, at, 0);
    held.given.insert("BTC".into(), 1_300);
    let stamps = |held: &StandingBooks, sent| {
        held.redate(ping(0, sent, LATE)).into_iter().map(|r| r.quote.ts).collect::<Vec<_>>()
    };
    assert_eq!(stamps(&held, at + MS * 300), Vec::<Millis>::new());
    assert_eq!(stamps(&held, at + MS * 301), [1_301]);
}

#[test]
fn a_book_read_after_a_given_stamp_is_raised_to_it_unless_a_closure_lies_between() {
    // Market 40 closes from `T + 1_000` to `T + 2_000`; market 1 is always open.
    const T: Millis = 1_793_000_000_000;
    let mut feed = feed(&[(1, "BTC"), (40, "AAPL")]);
    feed.schedules = Arc::new(BTreeMap::from([(40, around_the_clock(Some((T + 1_000, T + 2_000))))]));
    feed.standing.given.insert("BTC".into(), T + 2_200);
    feed.standing.given.insert("AAPL".into(), T + 2_200);
    // Stamped before the given stamp: raised to it, unless the market closed in between.
    assert_eq!(feed.after_given("BTC", quote("100", T + 1_500)), quote("100", T + 2_200));
    assert_eq!(feed.after_given("AAPL", quote("100", T + 2_100)), quote("100", T + 2_200));
    assert_eq!(feed.after_given("AAPL", quote("100", T + 1_500)), quote("100", T + 1_500));
    assert_eq!(feed.after_given("AAPL", quote("100", T + 500)), quote("100", T + 500));
    // Stamped after it, or for a symbol given none: its own stamp.
    assert_eq!(feed.after_given("BTC", quote("100", T + 2_400)), quote("100", T + 2_400));
    feed.standing.given.remove("BTC");
    assert_eq!(feed.after_given("BTC", quote("100", T + 1_500)), quote("100", T + 1_500));
}

#[test]
fn a_connection_that_does_not_redate_holds_nothing_and_a_new_one_starts_empty() {
    let mut probe = DelayProbe::default();
    let payload = probe.sent(Instant::now(), 1_000);
    let mut held = StandingBooks::default();
    held.connected(true);
    held.hold("BTC", &quote("100", 1_000), Some(&probe));
    assert!(held.books.is_empty(), "held before any ping was answered");
    probe.answered(&payload);
    probe.sent(Instant::now(), 1_001);
    held.connected(false);
    held.hold("BTC", &quote("100", 1_000), Some(&probe));
    assert!(held.books.is_empty(), "held on a connection that does not re-date");
    held.connected(true);
    held.hold("BTC", &quote("100", 1_000), None);
    assert!(held.books.is_empty(), "held on a connection without pings");
    held.hold("BTC", &quote("100", 1_000), Some(&probe));
    assert_eq!(held.books["BTC"].next_ping, 2, "read before the next ping, numbered 2");
    assert_eq!(Some(held.books["BTC"].vouched_from), probe.newest_answered());
    held.given.insert("BTC".into(), 1_300);
    held.connected(true);
    assert!(held.books.is_empty(), "a new connection holds no book");
    assert_eq!(held.given["BTC"], 1_300, "the stamps given outlive the connection");
}

#[test]
fn only_venues_measured_to_answer_pings_in_order_redate() {
    let measured = [Venue::Bitstamp, Venue::Coinbase, Venue::Kraken];
    let others = [Venue::Massive, Venue::Tradermade, Venue::Databento];
    measured.into_iter().for_each(|venue| assert!(redates(venue), "{venue:?}"));
    others.into_iter().for_each(|venue| assert!(!redates(venue), "{venue:?}"));
}

// ── Helpers ──

/// A Kraken quote for `mid` stamped `ts`.
fn quote(mid: &str, ts: Millis) -> SourceQuote {
    SourceQuote { source: "kraken".into(), mid: mid.parse().expect("mid"), ts, contract: None }
}

/// Ping `number`, sent at `sent` when the host clock read `sent_ms`.
fn ping(number: u64, sent: Instant, sent_ms: Millis) -> Ping {
    Ping { ping: number, sent, sent_ms }
}

/// A Kraken feed for `symbols`, connected to nothing.
fn feed(symbols: &[(MarketId, &str)]) -> VenueFeed {
    let symbols = symbols.iter().map(|(market, symbol)| VenueSymbol {
        market: *market,
        symbol: (*symbol).into(),
        contract: None,
    });
    let conn = VenueConnection {
        venue: Venue::Kraken,
        url: "ws://127.0.0.1:1".into(),
        api_key_env: None,
        api_key_secret: None,
        dataset: None,
        symbols: symbols.collect(),
    };
    VenueFeed::new(conn, None, fast(WAIT), tls_config().expect("tls config"), mpsc::channel(1).0)
}
