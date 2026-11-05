use std::sync::Arc;

use engine_core::Fixed;
use engine_core::command::Command;
use tokio::sync::mpsc;

use super::*;
use crate::clock::now_ms;
use crate::config::{VenueConnection, VenueSymbol};
use crate::feeds::tests::fake::{FakeEngine, WAIT, fast};
use crate::feeds::tests::rwa_support::around_the_clock;
use crate::feeds::venue::adapter::Adapter;
use crate::feeds::venue::setup::tls_config;

const MS: Duration = Duration::from_millis(1);
/// A host clock far past every stamp below, so it caps none.
const LATE: Millis = 1_000_000;

#[test]
fn a_book_read_before_an_answered_ping_is_stamped_at_the_ping() {
    let at = Instant::now();
    let mut held = StandingBooks::default();
    held.hold_at("BTC", quote("100", 1_000), 1_000, at, at, 1);
    // The ping went out 300 ms after the read, so the venue showed the book 300 ms past its stamp.
    let redated = held.redate(ping(1, at + MS * 300, LATE));
    assert_eq!(
        redated,
        [Redated { symbol: "BTC".into(), read_ts: 1_000, venue_ts: 1_000, quote: quote("100", 1_300) }]
    );
}

#[test]
fn a_book_read_after_the_ping_went_out_is_not_redated_by_its_answer() {
    let at = Instant::now();
    let mut held = StandingBooks::default();
    // Read after ping 1 went out and before ping 2.
    held.hold_at("BTC", quote("100", 1_000), 1_000, at + MS * 100, at, 2);
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
    held.hold_at("BTC", quote("100", 1_000), 1_000, at, at, 0);
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
    held.hold_at("BTC", quote("100", 1_000), 1_000, at, at, 0);
    held.hold_at("ETH", quote("10", 1_000), 1_000, at + MS * 400, at, 0);
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
    held.hold_at("BTC", quote("100", 1_000), 1_000, at, at, 0);
    held.hold_at("ETH", quote("10", 1_000), 1_000, at, at, 0);
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
    held.hold_at("BTC", quote("100", 1_000), 1_000, at, at, 0);
    held.hold_at("BTC", quote("101", 1_200), 1_200, at + MS * 200, at, 0);
    let redated = held.redate(ping(0, at + MS * 300, LATE));
    assert_eq!(
        redated,
        [Redated { symbol: "BTC".into(), read_ts: 1_200, venue_ts: 1_200, quote: quote("101", 1_300) }]
    );
}

#[test]
fn a_stamp_no_later_than_one_given_the_symbol_is_left_out() {
    let at = Instant::now();
    let mut held = StandingBooks::default();
    held.hold_at("BTC", quote("100", 1_000), 1_000, at, at, 0);
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
    let mut feed = feed(Venue::Kraken, &[(1, "BTC"), (40, "AAPL")]);
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
    held.hold("BTC", &quote("100", 1_000), LATE, &probe);
    assert!(held.books.is_empty(), "held before any ping was answered");
    probe.answered(&payload);
    probe.sent(Instant::now(), 1_001);
    held.connected(false);
    held.hold("BTC", &quote("100", 1_000), LATE, &probe);
    assert!(held.books.is_empty(), "held on a connection that does not re-date");
    held.connected(true);
    held.hold("BTC", &quote("100", 1_000), LATE, &probe);
    assert_eq!(held.books["BTC"].next_ping, 2, "read before the next ping, numbered 2");
    assert_eq!(Some(held.books["BTC"].vouched_from), probe.newest_answered());
    held.given.insert("BTC".into(), 1_300);
    held.connected(true);
    assert!(held.books.is_empty(), "a new connection holds no book");
    assert_eq!(held.given["BTC"], 1_300, "the stamps given outlive the connection");
}

#[test]
fn a_book_stamped_ahead_of_the_host_clock_is_redated_from_the_stamp_the_engine_stores() {
    // The venue's clock leads the host's by 800 ms. The engine stores the book at the host time it
    // was read, 1_000, so the answer to a ping sent 300 ms later refreshes it.
    let mut probe = DelayProbe::default();
    let payload = probe.sent(Instant::now(), 900);
    probe.answered(&payload);
    let mut held = StandingBooks::default();
    held.connected(true);
    held.hold("BTC", &quote("100", 1_800), 1_000, &probe);
    let read_at = held.books["BTC"].read_at;
    let redated = held.redate(ping(1, read_at + MS * 300, 1_300));
    assert_eq!(
        redated,
        [Redated { symbol: "BTC".into(), read_ts: 1_000, venue_ts: 1_800, quote: quote("100", 1_300) }]
    );
}

#[test]
fn a_book_read_after_a_redated_one_is_buffered_at_the_stamp_given_in_its_place() {
    const T: Millis = 1_793_000_000_000;
    let mut feed = feed(Venue::Kraken, &[(1, "BTC")]);
    feed.standing.given.insert("BTC".into(), T + 450);
    let mut buffered = BTreeMap::from([("BTC".to_owned(), Pending::default())]);
    buffered.get_mut("BTC").expect("pending").add(quote("100", T + 450), T + 460).expect("in range");
    // A moved book stamped before the re-dated one is raised to its stamp, so it replaces it.
    assert_eq!(feed.take_book("BTC", quote("101", T + 400), T + 500, &mut buffered), Ok(T + 450));
    assert_eq!(buffered.get_mut("BTC").expect("pending").take(), Some((quote("101", T + 450), T + 450)));
}

#[test]
fn a_changed_book_replaces_the_redated_quote_the_engine_holds_after_the_host_clock_steps_back() {
    let mut engine = FakeEngine::new();
    let t = now_ms();
    let mut feed = feed(Venue::Kraken, &[(1, "BTC")]);
    feed.standing.given.insert("BTC".into(), t + 450);
    let mut buffered = BTreeMap::from([("BTC".to_owned(), Pending::default())]);
    let pending = buffered.get_mut("BTC").expect("pending");
    // The re-dated quote was read at host time `t + 460` and the engine holds it, sequenced then.
    pending.add(quote("100", t + 450), t + 460).expect("in range");
    let (redated, _) = pending.take().expect("waiting");
    engine.apply_at(t + 460, Command::Prices { market: 1, quotes: vec![redated] });

    // The host clock steps back 1.3 s, but sequenced time does not. A changed book stamped before the
    // re-dated quote is raised to its stamp, which does not lead the highest host time read, so it
    // replaces the quote the engine holds.
    assert_eq!(feed.take_book("BTC", quote("101", t + 100), t - 840, &mut buffered), Ok(t + 450));
    let (changed, _) = buffered.get_mut("BTC").expect("pending").take().expect("waiting");
    assert_eq!(changed, quote("101", t + 450));
    engine.apply_at(t + 470, Command::Prices { market: 1, quotes: vec![changed] });
    let (reply, held) = std::sync::mpsc::channel();
    engine.answer(Box::new(move |engine| reply.send(engine.markets()[&1].quotes["kraken"].mid).expect("receiver")));
    assert_eq!(held.recv().expect("an answer"), "101".parse().unwrap(), "the engine holds the changed mid");
}

#[test]
fn a_book_raised_past_the_lead_the_engine_allows_goes_at_its_own_stamp_in_place_of_the_waiting_quote() {
    const T: Millis = 1_793_000_000_000;
    let mut feed = feed(Venue::Kraken, &[(1, "BTC")]);
    feed.standing.given.insert("BTC".into(), T + 450);
    // No host time was read past the stepped-back clock, as on a new connection: nothing bounds
    // sequenced time from below.
    let mut buffered = BTreeMap::from([("BTC".to_owned(), Pending::default())]);
    buffered.get_mut("BTC").expect("pending").add(quote("100", T - 840), T - 840).expect("in range");
    // Raised to the given stamp, the moved book would lead the clock by 1_290 ms, so it goes at its
    // own stamp, 940 ms ahead, and the quote it changes waits no more.
    assert_eq!(feed.take_book("BTC", quote("101", T + 100), T - 840, &mut buffered), Ok(T + 100));
    assert_eq!(buffered.get_mut("BTC").expect("pending").take(), Some((quote("101", T + 100), T - 840)));
    // A book whose own stamp leads that far is refused, and its symbol's standing book forgotten.
    feed.standing.hold_at("BTC", quote("101", T + 100), T + 100, Instant::now(), Instant::now(), 0);
    assert_eq!(feed.take_book("BTC", quote("102", T + 500), T - 840, &mut buffered), Err(1_340));
    assert!(feed.standing.books.is_empty(), "the standing book is forgotten");
}

#[tokio::test]
async fn the_record_of_a_sequenced_quote_is_stamped_as_the_engine_stores_it_after_a_host_clock_step_back() {
    // A quote read at a host time 500 ms ahead of the clock now, as after a step back, is sequenced
    // at a time no earlier than that. The engine holds a changed mid at the stamp `Pending` gives
    // it, so the record names it, not the mid it replaced.
    let (tx, mut sequenced) = mpsc::channel(4);
    let mut feed = feed_into(Venue::Kraken, &[(1, "BTC")], tx);
    let hwm = now_ms() + 500;
    feed.sent.insert("BTC".into(), (Fixed::from_int(100), hwm));
    let mut buffered = BTreeMap::from([("BTC".to_owned(), Pending::default())]);
    let pending = buffered.get_mut("BTC").expect("pending");
    pending.add(quote("100", hwm), hwm).expect("in range");
    pending.take();
    // Read after the step back, stamped 20 ms past the high-water mark.
    pending.add(quote("101", hwm + 20), hwm - 500).expect("in range");
    feed.emit(&mut buffered, &mut BTreeMap::new()).await.expect("sequencer up");
    assert!(sequenced.try_recv().is_ok(), "the moved mid is sequenced");
    assert_eq!(feed.sent["BTC"], (Fixed::from_int(101), hwm), "the record names the mid the engine holds");
}

#[test]
fn an_answer_older_than_the_delay_bound_redates_nothing() {
    let (mut feed, adapter) = bitstamp();
    let now = now_ms();
    let read_at = Instant::now() - Duration::from_secs(3);
    feed.standing.hold_at("btcusd", quote("100", now - 3_000), now - 3_000, read_at, read_at, 0);
    let mut buffered = BTreeMap::new();
    // Sent 2.5 s ago, past the 2 s bound, its answer re-dates nothing.
    let late = ping(0, Instant::now() - Duration::from_millis(2_500), now - 2_500);
    assert!(!feed.redate_books(late, &adapter, &mut buffered));
    assert!(buffered.values().all(|pending| !pending.waiting()), "nothing buffered");
    // Sent 1 s ago, within it, it re-dates the book to when it went out.
    let timely = ping(0, Instant::now() - Duration::from_secs(1), now - 1_000);
    assert!(feed.redate_books(timely, &adapter, &mut buffered));
    let redated = buffered.get_mut("btcusd").expect("buffered").take().map(|(quote, _)| quote.ts);
    assert_eq!(redated, Some(now - 1_000));
}

#[test]
fn the_stamp_a_redated_book_is_given_is_recorded_and_raises_the_next_book_for_the_symbol() {
    let (mut feed, adapter) = bitstamp();
    let now = now_ms();
    let read_at = Instant::now() - Duration::from_secs(1);
    feed.standing.hold_at("btcusd", quote("100", now - 1_000), now - 1_000, read_at, read_at, 0);
    let mut buffered = BTreeMap::new();
    assert!(feed.redate_books(ping(0, Instant::now() - MS * 500, now - 500), &adapter, &mut buffered));
    assert_eq!(feed.standing.given_for("btcusd"), Some(now - 500), "the stamp is recorded");
    // The next book read for the symbol, stamped before it, is raised to it.
    assert_eq!(feed.after_given("btcusd", quote("101", now - 800)), quote("101", now - 500));
}

#[test]
fn a_book_a_leading_venue_clock_stamped_inside_a_closure_is_not_redated_into_the_session_before_it() {
    const AAPL: MarketId = 40;
    let (mut feed, adapter) = bitstamp_on(AAPL);
    let read = now_ms();
    // The market closes 600 ms after the book was read. The venue's clock leads the host's by 700
    // ms, so it stamped the book 100 ms into the closure, and it is held at the host time it was
    // read.
    let closure = read + 600;
    feed.schedules = Arc::new(BTreeMap::from([(AAPL, around_the_clock(Some((closure, closure + 60_000))))]));
    let read_at = Instant::now();
    feed.standing.hold_at("btcusd", quote("100", read), closure + 100, read_at, read_at, 0);
    let mut buffered = BTreeMap::new();
    // A ping sent 500 ms after the read, before the closure, is answered: the venue showed the book
    // then, but stamped it after the closure, so it is not a pre-closure quote.
    let answered = ping(0, read_at + MS * 500, read + 500);
    assert!(!feed.redate_books(answered, &adapter, &mut buffered));
    assert!(buffered.values().all(|pending| !pending.waiting()), "nothing buffered");
    assert!(feed.standing.books.is_empty(), "the book is forgotten");
    assert_eq!(feed.standing.given_for("btcusd"), None, "no stamp given");
}

#[test]
fn an_answer_for_a_held_back_symbol_forgets_its_book() {
    let (mut feed, adapter) = bitstamp();
    let now = now_ms();
    let read_at = Instant::now() - Duration::from_secs(1);
    feed.standing.hold_at("btcusd", quote("100", now - 1_000), now - 1_000, read_at, read_at, 0);
    feed.idle.insert("btcusd".into());
    let mut buffered = BTreeMap::new();
    let answered = ping(0, Instant::now(), now);
    assert!(!feed.redate_books(answered, &adapter, &mut buffered));
    assert!(buffered.values().all(|pending| !pending.waiting()), "nothing buffered");
    assert!(feed.standing.books.is_empty(), "the book is forgotten");
}

#[test]
fn a_redated_quote_dated_ahead_of_a_stepped_back_host_clock_is_refused_and_its_book_forgotten() {
    let (mut feed, adapter) = bitstamp();
    let now = now_ms();
    // Read 1 s ago at a host clock 1 s ahead of the current one, and pinged 900 ms later, before the
    // host clock stepped back 2 s: the re-dated stamp would lead it by 1.9 s.
    let read_at = Instant::now() - Duration::from_secs(1);
    feed.standing.hold_at("btcusd", quote("100", now + 1_000), now + 1_000, read_at, read_at, 0);
    let mut buffered = BTreeMap::new();
    let answered = ping(0, read_at + MS * 900, now + 2_000);
    assert!(!feed.redate_books(answered, &adapter, &mut buffered));
    assert!(buffered.values().all(|pending| !pending.waiting()), "nothing buffered");
    assert!(feed.standing.books.is_empty(), "the book is forgotten");
    assert_eq!(feed.standing.given_for("btcusd"), None, "no stamp given");
}

#[test]
fn only_venues_measured_to_answer_pings_in_order_redate() {
    let measured = [Venue::Bitstamp, Venue::Coinbase, Venue::Kraken];
    let others = [Venue::Massive, Venue::Tradermade];
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

/// A feed on `venue` for `symbols`, connected to nothing, sequencing into no one.
fn feed(venue: Venue, symbols: &[(MarketId, &str)]) -> VenueFeed {
    feed_into(venue, symbols, mpsc::channel(1).0)
}

/// `feed`, sequencing into `tx`.
fn feed_into(venue: Venue, symbols: &[(MarketId, &str)], tx: mpsc::Sender<crate::sequencer::Request>) -> VenueFeed {
    let symbols = symbols.iter().map(|(market, symbol)| VenueSymbol {
        market: *market,
        symbol: (*symbol).into(),
        contract: None,
    });
    let conn = VenueConnection {
        venue,
        url: "ws://127.0.0.1:1".into(),
        api_key_env: None,
        api_key_secret: None,
        symbols: symbols.collect(),
    };
    VenueFeed::new(conn, None, fast(WAIT), tls_config().expect("tls config"), tx)
}

/// A Bitstamp feed for `btcusd` (market 1) with a 2 s delay bound, and the adapter it reads with:
/// Bitstamp reads each top whole, so a standing book always tops out at its mid.
fn bitstamp() -> (VenueFeed, Adapter) {
    bitstamp_on(1)
}

/// `bitstamp`, pricing `market`.
fn bitstamp_on(market: MarketId) -> (VenueFeed, Adapter) {
    let mut feed = feed(Venue::Bitstamp, &[(market, "btcusd")]);
    feed.timing.max_delay = Duration::from_secs(2);
    let adapter = Adapter::new(&feed.conn, None, &["btcusd"]);
    (feed, adapter)
}
