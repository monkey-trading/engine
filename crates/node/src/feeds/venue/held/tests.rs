use super::*;

const MS: Duration = Duration::from_millis(1);
/// A host clock far past every stamp below, so it caps none.
const LATE: Millis = 1_000_000;

#[test]
fn a_book_read_before_an_answered_ping_is_stamped_at_the_ping() {
    let at = Instant::now();
    let mut held = HeldBooks::default();
    held.hold_at("BTC", quote("100", 1_000), at, 1);
    // The ping went out 300 ms after the read, so the venue showed the book 300 ms past its stamp.
    let redated = held.redate(1, at + MS * 300, LATE);
    assert_eq!(redated, [Redated { symbol: "BTC".into(), read_ts: 1_000, quote: quote("100", 1_300) }]);
}

#[test]
fn a_book_read_after_the_ping_went_out_is_not_redated_by_its_answer() {
    let at = Instant::now();
    let mut held = HeldBooks::default();
    // Read after ping 1 went out and before ping 2.
    held.hold_at("BTC", quote("100", 1_000), at + MS * 100, 2);
    assert_eq!(held.redate(1, at, LATE), []);
    // A pong answering ping 2 or a later one vouches for it.
    let stamps = |ping| held.redate(ping, at + MS * 400, LATE).iter().map(|r| r.quote.ts).collect::<Vec<_>>();
    assert_eq!(stamps(2), [1_300]);
    assert_eq!(stamps(3), [1_300]);
}

#[test]
fn a_redated_stamp_never_passes_the_host_clock_and_must_gain_on_the_book() {
    let at = Instant::now();
    let mut held = HeldBooks::default();
    held.hold_at("BTC", quote("100", 1_000), at, 0);
    let stamps = |sent, now| held.redate(0, sent, now).iter().map(|r| r.quote.ts).collect::<Vec<_>>();
    assert_eq!(stamps(at + MS * 300, 1_100), [1_100]);
    // A host clock at or behind the book's stamp, or a ping sent as it was read, adds nothing.
    assert_eq!(stamps(at + MS * 300, 1_000), Vec::<Millis>::new());
    assert_eq!(stamps(at + MS * 300, 900), Vec::<Millis>::new());
    assert_eq!(stamps(at, LATE), Vec::<Millis>::new());
}

#[test]
fn books_are_redated_up_to_max_book_silence_and_forgotten_past_it() {
    let at = Instant::now();
    let mut held = HeldBooks::default();
    held.hold_at("BTC", quote("100", 1_000), at, 0);
    held.hold_at("ETH", quote("10", 1_000), at + MS, 0);
    let edge = at + MAX_BOOK_SILENCE;
    assert_eq!(held.expire(edge), Vec::<String>::new());
    let silence = Millis::try_from(MAX_BOOK_SILENCE.as_millis()).expect("in range");
    let stamps =
        |held: &HeldBooks, sent| held.redate(0, sent, LATE).into_iter().map(|r| r.quote.ts).collect::<Vec<_>>();
    assert_eq!(stamps(&held, edge), [1_000 + silence, 1_000 + silence - 1]);
    // A millisecond on, BTC's book is past it and ETH's, read a millisecond later, is not.
    assert_eq!(held.expire(edge + MS), ["BTC"]);
    let redated = held.redate(0, edge + MS, LATE);
    assert_eq!(redated.iter().map(|r| r.symbol.as_str()).collect::<Vec<_>>(), ["ETH"]);
}

#[test]
fn a_forgotten_or_cleared_book_is_not_redated() {
    let at = Instant::now();
    let mut held = HeldBooks::default();
    held.hold_at("BTC", quote("100", 1_000), at, 0);
    held.hold_at("ETH", quote("10", 1_000), at, 0);
    held.forget("BTC");
    let symbols =
        |held: &HeldBooks| held.redate(0, at + MS * 100, LATE).into_iter().map(|r| r.symbol).collect::<Vec<_>>();
    assert_eq!(symbols(&held), ["ETH"]);
    held.clear();
    assert_eq!(symbols(&held), Vec::<String>::new());
}

#[test]
fn a_newer_book_replaces_the_one_held() {
    let at = Instant::now();
    let mut held = HeldBooks::default();
    held.hold_at("BTC", quote("100", 1_000), at, 0);
    held.hold_at("BTC", quote("101", 1_200), at + MS * 200, 0);
    let redated = held.redate(0, at + MS * 300, LATE);
    assert_eq!(redated, [Redated { symbol: "BTC".into(), read_ts: 1_200, quote: quote("101", 1_300) }]);
}

#[test]
fn a_stamp_no_later_than_one_given_the_symbol_is_left_out_and_raises_the_next_book() {
    let at = Instant::now();
    let mut held = HeldBooks::default();
    held.hold_at("BTC", quote("100", 1_000), at, 0);
    held.given.insert("BTC".into(), 1_300);
    let stamps =
        |held: &HeldBooks, sent| held.redate(0, sent, LATE).into_iter().map(|r| r.quote.ts).collect::<Vec<_>>();
    assert_eq!(stamps(&held, at + MS * 300), Vec::<Millis>::new());
    assert_eq!(stamps(&held, at + MS * 301), [1_301]);
    // A book read after it is stamped at the given stamp at the earliest; one stamped later keeps its
    // own, and another symbol's is untouched.
    assert_eq!(held.after_given("BTC", quote("101", 1_100)), quote("101", 1_300));
    assert_eq!(held.after_given("BTC", quote("101", 1_400)), quote("101", 1_400));
    assert_eq!(held.after_given("ETH", quote("10", 1_100)), quote("10", 1_100));
}

#[test]
fn a_connection_that_does_not_redate_holds_nothing_and_a_new_one_starts_empty() {
    let mut probe = DelayProbe::default();
    probe.sent(Instant::now());
    let mut held = HeldBooks::default();
    held.connected(false);
    held.hold("BTC", &quote("100", 1_000), Some(&probe));
    assert!(held.books.is_empty(), "held on a connection that does not re-date");
    held.connected(true);
    held.hold("BTC", &quote("100", 1_000), None);
    assert!(held.books.is_empty(), "held on a connection without pings");
    held.hold("BTC", &quote("100", 1_000), Some(&probe));
    assert_eq!(held.books["BTC"].next_ping, 1, "read before the next ping, numbered 1");
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
