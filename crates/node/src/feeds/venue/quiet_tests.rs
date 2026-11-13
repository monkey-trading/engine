use std::collections::VecDeque;

use engine_core::command::Command;
use engine_core::market::PriceRules;
use engine_core::price::{SourceQuote, compute_index};

use super::*;
use crate::clock::now_ms;
use crate::config::{Venue, VenueSymbol};
use crate::feeds::tests::fake::fast;
use crate::feeds::venue::quotes::{Verdict, record, refresh_after, verdict};

/// The price rules of every dev and production market of the live venues.
const RULES: PriceRules = PriceRules { max_age_ms: 1_000, max_divergence_bps: 25, min_sources: 2 };
const SYMBOL: &str = "BTC/USD";

#[test]
fn a_repeated_mid_waits_until_half_the_shorter_limit_has_passed() {
    // Half the shorter of `stale_after` and the market's `max_age_ms`.
    assert_eq!(refresh_after(1_000, 1_000), 500);
    assert_eq!(refresh_after(1_000, 250), 125);
    assert_eq!(refresh_after(300, 1_000), 150);
    assert_eq!(refresh_after(1, 1), 0);

    let sent = Some((Fixed::from_int(100), 10_000));
    let refresh = Some(500);
    // A new mid goes at once, wherever its stamp falls.
    assert_eq!(verdict(sent, &at(101, 10_001), refresh, 10_002), Verdict::Send);
    assert_eq!(verdict(None, &at(100, 10_001), refresh, 10_002), Verdict::Send);
    // A repeat waits until the sent quote's stamp is half the limit old by the host clock, however
    // late the repeat's own stamp, and goes the moment it is.
    assert_eq!(verdict(sent, &at(100, 10_300), refresh, 10_300), Verdict::Wait(10_500));
    assert_eq!(verdict(sent, &at(100, 10_300), refresh, 10_499), Verdict::Wait(10_500));
    assert_eq!(verdict(sent, &at(100, 10_300), refresh, 10_500), Verdict::Send);
    // A repeat that arrives late, its stamp already past the refresh time, goes at once.
    assert_eq!(verdict(sent, &at(100, 10_100), refresh, 10_900), Verdict::Send);
    // The same stamp adds nothing; one dated before the sent quote is no repeat to hold back.
    assert_eq!(verdict(sent, &at(100, 10_000), refresh, 10_001), Verdict::Redundant);
    assert_eq!(verdict(sent, &at(100, 9_999), refresh, 10_001), Verdict::Send);
    // Until the market's rules are known, nothing waits.
    assert_eq!(verdict(sent, &at(100, 10_300), None, 10_300), Verdict::Send);
}

#[tokio::test]
async fn a_repeat_waits_for_its_refresh_and_arms_the_timer_for_it() {
    let (mut feed, mut rx) = feed_with_rules(true);
    let first = at(100, now_ms() - 10);
    assert_eq!(emit_one(&mut feed, first.clone()).await, None);
    assert_eq!(sequenced(&mut rx), Some(first));

    // The same mid again, stamped now: held, with the timer set for the first quote's stamp plus 500ms.
    let before = Instant::now();
    let wake = emit_one(&mut feed, at(100, now_ms())).await.expect("a timer for the repeat");
    assert_eq!(sequenced(&mut rx), None);
    let wait = wake - before;
    assert!(wait > Duration::from_millis(400) && wait <= Duration::from_millis(500), "{wait:?}");
}

#[tokio::test]
async fn a_repeat_is_held_for_half_the_markets_max_age_when_that_is_shorter_than_stale_after() {
    let (mut feed, mut rx) = feed_with_rules(true);
    feed.max_age.insert(1, 400);
    emit_one(&mut feed, at(100, now_ms() - 10)).await;
    assert!(sequenced(&mut rx).is_some());

    let before = Instant::now();
    let wake = emit_one(&mut feed, at(100, now_ms())).await.expect("a timer for the repeat");
    assert_eq!(sequenced(&mut rx), None);
    let wait = wake - before;
    assert!(wait > Duration::from_millis(150) && wait <= Duration::from_millis(200), "{wait:?}");
}

#[tokio::test]
async fn a_repeat_is_released_before_its_delay_bound_runs_out() {
    let (mut feed, mut rx) = feed_with_rules(true);
    let max_delay = Duration::from_millis(1_200);
    feed.timing.max_delay = max_delay;
    // The newest answered ping is 900ms old, none answered since: a quote read now stops counting
    // 300ms from now, before the refresh of one held now falls due 500ms after it was sequenced.
    let mut probe = DelayProbe::default();
    let payload = probe.sent(Instant::now() - Duration::from_millis(900), now_ms() - 900);
    probe.answered(&payload);
    feed.delay = probe;
    let mut buffered = BTreeMap::from([(SYMBOL.to_owned(), Pending::default())]);

    // The first quote goes at once, as a repeat never waits on it.
    let first = at(100, now_ms() - 10);
    buffered.get_mut(SYMBOL).unwrap().add(first.clone(), now_ms()).unwrap();
    note(&mut feed.delay, &first, max_delay);
    assert_eq!(feed.emit(&mut buffered, &mut BTreeMap::new()).await.unwrap(), None);
    assert!(sequenced(&mut rx).is_some());

    // The same mid again: held, but the bound noted when it was read runs out before the refresh
    // falls due, so the timer is set inside the bound less the send margin, not at the refresh.
    let second = at(100, now_ms());
    buffered.get_mut(SYMBOL).unwrap().add(second.clone(), now_ms()).unwrap();
    note(&mut feed.delay, &second, max_delay);
    let noted = feed.delay.peek_not_after(SYMBOL, second.ts).expect("a bound noted");
    let before = Instant::now();
    let wake = feed.emit(&mut buffered, &mut BTreeMap::new()).await.unwrap().expect("a timer for the repeat");
    assert!(sequenced(&mut rx).is_none(), "the repeat waits");
    let wait = wake - before;
    assert!(wait > Duration::from_millis(100) && wait <= Duration::from_millis(300), "{wait:?}");

    // Released there, the repeat sequences inside its bound instead of dropping as past it.
    tokio::time::sleep_until(wake).await;
    assert!(feed.emit(&mut buffered, &mut BTreeMap::new()).await.unwrap().is_none());
    let (quote, bound) = sequenced_with_bound(&mut rx).expect("the repeat sequences");
    assert_eq!(quote.mid, second.mid);
    assert_eq!(bound, noted, "the repeat carries the bound it was read with");
    assert!(feed.emit(&mut buffered, &mut BTreeMap::new()).await.unwrap().is_none());
    assert!(sequenced(&mut rx).is_none());
}

#[tokio::test]
async fn a_repeat_within_the_send_margin_of_its_bound_goes_at_once() {
    let (mut feed, mut rx) = feed_with_rules(true);
    let max_delay = Duration::from_millis(1_200);
    feed.timing.max_delay = max_delay;
    // The newest answered ping is 1.1s old: a quote read now stops counting in 100ms, at the send
    // margin, so one held for its refresh would reach the sequencer only past its bound.
    let mut probe = DelayProbe::default();
    let payload = probe.sent(Instant::now() - Duration::from_millis(1_100), now_ms() - 1_100);
    probe.answered(&payload);
    feed.delay = probe;
    let mut buffered = BTreeMap::from([(SYMBOL.to_owned(), Pending::default())]);

    let first = at(100, now_ms() - 10);
    buffered.get_mut(SYMBOL).unwrap().add(first.clone(), now_ms()).unwrap();
    note(&mut feed.delay, &first, max_delay);
    feed.emit(&mut buffered, &mut BTreeMap::new()).await.unwrap();
    assert!(sequenced(&mut rx).is_some());

    let second = at(100, now_ms());
    buffered.get_mut(SYMBOL).unwrap().add(second.clone(), now_ms()).unwrap();
    note(&mut feed.delay, &second, max_delay);
    let noted = feed.delay.peek_not_after(SYMBOL, second.ts).expect("a bound noted");
    assert_eq!(feed.emit(&mut buffered, &mut BTreeMap::new()).await.unwrap(), None);
    let (quote, bound) = sequenced_with_bound(&mut rx).expect("the repeat goes at once");
    assert_eq!(quote.mid, second.mid);
    assert_eq!(bound, noted, "it still carries the bound it was read with");
}

#[tokio::test]
async fn a_quote_the_engine_does_not_hold_is_forgotten_so_the_next_goes_at_once() {
    use crate::feeds::venue::confirm::Confirmation;
    let (mut feed, mut rx) = feed_with_rules(true);
    let first = at(100, now_ms() - 10);
    emit_one(&mut feed, first.clone()).await;
    assert_eq!(sequenced(&mut rx), Some(first.clone()));
    let sent = *feed.sent.get(SYMBOL).expect("the quote is on record");
    let asked = |held: Option<(Fixed, Millis)>| Confirmation {
        sent: BTreeMap::from([(SYMBOL.to_owned(), sent)]),
        held: held.map(|quote| (SYMBOL.to_owned(), quote)).into_iter().collect(),
        ..Confirmation::default()
    };

    // The engine holds it: the record stays, as it does for a quote the engine holds a newer stamp of.
    feed.confirmed(asked(Some(sent)));
    feed.confirmed(asked(Some((sent.0, sent.1 + 5))));
    assert_eq!(feed.sent.get(SYMBOL), Some(&sent));
    // The engine holds another mid, or an older stamp of this one, or nothing, though the feed had
    // sent it ahead of the question: the sequencer dropped it. The record goes.
    for held in [Some((Fixed::from_int(99), sent.1 + 5)), Some((sent.0, sent.1 - 5)), None] {
        feed.sent.insert(SYMBOL.to_owned(), sent);
        feed.confirmed(asked(held));
        assert_eq!(feed.sent.get(SYMBOL), None, "{held:?}");
    }
    // A quote sent after the question was asked may still be on its way: its record stays.
    feed.sent.insert(SYMBOL.to_owned(), (sent.0, sent.1 + 20));
    feed.confirmed(asked(None));
    assert_eq!(feed.sent.get(SYMBOL), Some(&(sent.0, sent.1 + 20)));
    // With no record the same mid goes at once, though the engine's quote is fresh by the clock.
    feed.sent.remove(SYMBOL);
    let again = at(100, now_ms());
    emit_one(&mut feed, again.clone()).await;
    assert_eq!(sequenced(&mut rx), Some(again));
}

#[tokio::test]
async fn a_move_dated_before_the_record_leaves_the_record_so_the_next_move_goes_at_once() {
    let (mut feed, mut rx) = feed_with_rules(true);
    emit_one(&mut feed, at(100, now_ms() - 10)).await;
    assert!(sequenced(&mut rx).is_some());
    let on_record = *feed.sent.get(SYMBOL).expect("the quote is on record");

    // A move dated before the record goes, but the engine keeps the quote it holds, so the record
    // stays; a later move to the same mid is no repeat of anything the engine holds.
    let older = at(101, now_ms() - 300);
    emit_one(&mut feed, older.clone()).await;
    assert_eq!(sequenced(&mut rx), Some(older));
    assert_eq!(feed.sent.get(SYMBOL), Some(&on_record));
    let next = at(101, now_ms());
    emit_one(&mut feed, next.clone()).await;
    assert_eq!(sequenced(&mut rx), Some(next));
}

#[tokio::test]
async fn nothing_waits_before_the_engine_has_confirmed_the_markets_rules() {
    let (mut feed, mut rx) = feed_with_rules(false);
    for ts in [now_ms() - 20, now_ms() - 10] {
        emit_one(&mut feed, at(100, ts)).await;
        assert_eq!(sequenced(&mut rx).map(|quote| quote.ts), Some(ts));
    }
}

#[tokio::test]
async fn the_first_quote_after_a_hold_goes_at_once_whatever_the_engine_holds() {
    let (mut feed, mut rx) = feed_with_rules(true);
    emit_one(&mut feed, at(100, now_ms() - 20)).await;
    assert!(sequenced(&mut rx).is_some());

    // The symbol is held back (its market closed, or its contract idle): its quotes are dropped,
    // and the memory of what was sequenced goes, since the engine's quote ages out meanwhile.
    feed.idle.insert(SYMBOL.to_owned());
    emit_one(&mut feed, at(100, now_ms() - 10)).await;
    assert_eq!(sequenced(&mut rx), None);
    assert!(feed.sent.is_empty());

    // Taken again, the same mid goes at once, not after a refresh time.
    feed.idle.clear();
    let again = at(100, now_ms());
    emit_one(&mut feed, again.clone()).await;
    assert_eq!(sequenced(&mut rx), Some(again));
}

#[test]
fn the_index_is_never_worse_and_without_a_leading_venue_clock_the_same_at_every_instant() {
    // A venue clock behind, level with and ahead of the host's by up to the engine's limit.
    for skew in [-100, 0, 600, 1_000] {
        for seed in [0x9E37_79B9_7F4A_7C15, 0xD1B5_4A32_D192_ED03, 0x1234_5678_9ABC_DEF1] {
            // The sequencer stalls for up to 500ms, which the 500ms the refresh leaves covers.
            let outcome = simulate(seed, 500, skew);
            let case = format!("seed {seed:#x}, skew {skew}");
            assert!(outcome.priced > 20_000, "only {} priced, {case}: too few to compare", outcome.priced);
            // However the clocks differ, the quiet world never loses the index and never shows
            // another mid.
            assert_eq!(outcome.quiet_unpriced_alone, Vec::<Millis>::new(), "{case}");
            assert_eq!(outcome.mids_differing, Vec::<Millis>::new(), "{case}");
            // Without a clock ahead it is the same index at every instant. A venue clock ahead
            // has the engine date a quote that waited out a stall at its release, so the
            // every-update world can lose the index where the quiet one, which sent less into the
            // stall, keeps it.
            if skew <= 0 {
                assert_eq!(outcome.differing, Vec::<Millis>::new(), "{case}");
            }
            assert!(outcome.quiet * 100 < outcome.every_update * 55, "{} of {}", outcome.quiet, outcome.every_update);
        }
    }
}

#[test]
fn a_sequencer_stall_past_the_refresh_margin_unprices_the_market_the_every_update_world_keeps() {
    // The margin is real: past it a quote ages out before its refresh is applied.
    let outcome = simulate(0x9E37_79B9_7F4A_7C15, 800, 0);
    assert!(!outcome.quiet_unpriced_alone.is_empty());
}

#[tokio::test]
async fn a_quote_stamped_ahead_of_the_host_clock_is_sequenced_at_once_and_refreshed_by_the_stamp_the_engine_keeps() {
    let (mut feed, mut rx) = feed_with_rules(true);
    let ahead = at(100, now_ms() + 400);
    assert_eq!(emit_one(&mut feed, ahead.clone()).await, None);
    assert_eq!(sequenced(&mut rx), Some(ahead));

    // The engine keeps that quote stamped at the time it was sequenced, so its refresh falls due
    // 500ms after the send, not 500ms after the stamp.
    tokio::time::sleep(Duration::from_millis(3)).await;
    let before = Instant::now();
    let wake = emit_one(&mut feed, at(100, now_ms() + 400)).await.expect("a timer for the repeat");
    assert_eq!(sequenced(&mut rx), None);
    let wait = wake - before;
    assert!(wait > Duration::from_millis(400) && wait <= Duration::from_millis(500), "{wait:?}");
}

#[tokio::test]
async fn a_symbol_sequences_a_burst_at_once_then_one_quote_per_interval() {
    let (mut feed, mut rx) = feed_with_rules(true);
    let mut buffered = BTreeMap::from([(SYMBOL.to_owned(), Pending::default())]);
    let mut spent = BTreeMap::new();
    let mut sent = 0;
    let mut wake = None;
    // Twenty moves of the mid in a row.
    for mid in 100..120 {
        buffered.get_mut(SYMBOL).unwrap().add(at(mid, now_ms()), now_ms()).unwrap();
        wake = feed.emit(&mut buffered, &mut spent).await.unwrap();
        sent += usize::from(sequenced(&mut rx).is_some());
    }
    // The burst went as it arrived; the rest coalesced behind the budget, which wakes the timer.
    assert_eq!(sent, 8);
    let wait = wake.expect("a timer for the quote the budget holds") - Instant::now();
    assert!(wait <= feed.timing.interval, "{wait:?}");
    // Once the budget refills, the newest goes, and only that.
    tokio::time::sleep(feed.timing.interval * 2).await;
    feed.emit(&mut buffered, &mut spent).await.unwrap();
    assert_eq!(sequenced(&mut rx).map(|quote| quote.mid), Some(Fixed::from_int(119)));
    assert_eq!(sequenced(&mut rx), None);
}

// ── Helpers ──

fn at(mid: i64, ts: Millis) -> SourceQuote {
    SourceQuote { source: "kraken".into(), mid: Fixed::from_int(mid), ts, contract: None }
}

/// A Kraken feed for `SYMBOL` on market 1 that is never run, sequencing into the returned channel.
fn feed_with_rules(known: bool) -> (VenueFeed, mpsc::Receiver<Request>) {
    let symbols = vec![VenueSymbol { market: 1, symbol: SYMBOL.into(), contract: None }];
    let conn = VenueConnection {
        venue: Venue::Kraken,
        url: "wss://ws.kraken.com/v2".into(),
        api_key_env: None,
        api_key_secret: None,
        oauth: None,
        symbols,
    };
    let (tx, rx) = mpsc::channel(16);
    let mut feed = VenueFeed::new(conn, Credentials::Public, fast(Duration::from_secs(5)), tls_config().unwrap(), tx);
    if known {
        feed.max_age.insert(1, RULES.max_age_ms);
    }
    (feed, rx)
}

/// Emits `quote` for the one symbol with no pace in the way, returning when the emit timer should
/// next wake.
async fn emit_one(feed: &mut VenueFeed, quote: SourceQuote) -> Option<Instant> {
    let mut buffered = BTreeMap::from([(SYMBOL.to_owned(), Pending::default())]);
    buffered.get_mut(SYMBOL).unwrap().add(quote, now_ms()).unwrap();
    feed.emit(&mut buffered, &mut BTreeMap::new()).await.unwrap()
}

fn sequenced(rx: &mut mpsc::Receiver<Request>) -> Option<SourceQuote> {
    sequenced_with_bound(rx).map(|(quote, _)| quote)
}

/// The next sequenced quote and the delay bound it carries, if one went.
fn sequenced_with_bound(rx: &mut mpsc::Receiver<Request>) -> Option<(SourceQuote, std::time::Instant)> {
    match rx.try_recv() {
        Ok(Request::Quote { cmd: Command::Prices { mut quotes, .. }, not_after }) => {
            quotes.pop().map(|quote| (quote, not_after))
        }
        Ok(_) => panic!("expected a price command"),
        Err(_) => None,
    }
}

/// Notes the delay bound `receive` notes for `quote`, as if it had just been read off the wire.
fn note(probe: &mut DelayProbe, quote: &SourceQuote, max_delay: Duration) {
    probe.note_read(SYMBOL, quote.ts, max_delay);
}

/// A pseudo-random sequence, so every run replays the same venues.
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

/// Two venues' updates (the crypto markets' sources, with `min_sources` 2, so one aged-out quote
/// unprices the market) as they reach the host, sorted by arrival: a book update every few tens of
/// milliseconds, its mid moving one time in five, now and then a venue quiet for up to a second,
/// stamped `skew` ahead of the host clock, and a delay between stamp and arrival that wanders up
/// to 600ms and spikes to 1,050ms.
fn venue_updates(rng: &mut Rng, until: Millis, skew: Millis) -> Vec<(Millis, usize, SourceQuote)> {
    let mut updates = Vec::new();
    for (index, source) in ["a", "b"].into_iter().enumerate() {
        let (mut stamp, mut arrival, mut delay, mut mid) = (1_000, 1_000, 40, Fixed::from_int(100_000));
        while stamp < until {
            let quiet = rng.below(200) == 0;
            stamp += if quiet { 400 + rng.below(900) as Millis } else { 1 + rng.below(60) as Millis };
            if rng.below(5) == 0 {
                mid += Fixed::from_int(if rng.below(2) == 0 { 1 } else { -1 });
            }
            delay = (delay + rng.below(81) as Millis - 40).clamp(0, 600);
            let spike = if rng.below(300) == 0 { 600 + rng.below(450) as Millis } else { delay };
            // A venue's updates arrive in the order it stamped them.
            arrival = arrival.max(stamp + spike);
            let quote = SourceQuote { source: source.into(), mid, ts: stamp + skew, contract: None };
            updates.push((arrival, index, quote));
        }
    }
    updates.sort_by_key(|(arrival, index, _)| (*arrival, *index));
    updates
}

/// A sequencer that applies commands in order, holding them all through its stalls, and the quotes
/// the engine holds as a result.
#[derive(Default)]
struct Applied {
    queue: VecDeque<(Millis, SourceQuote)>,
    held: BTreeMap<String, SourceQuote>,
}

impl Applied {
    /// Queues a command sent at `now`: applied at once, or when the stall it is sent in ends.
    fn send(&mut self, now: Millis, quote: SourceQuote, stalls: &[(Millis, Millis)]) {
        let at = stalls.iter().find(|(from, to)| (*from..*to).contains(&now)).map_or(now, |(_, to)| *to);
        self.queue.push_back((at, quote));
    }

    /// Applies what is due at `now`, storing each quote as the engine's `store_quotes` does.
    fn apply(&mut self, now: Millis) {
        while self.queue.front().is_some_and(|(at, _)| *at <= now) {
            let (at, quote) = self.queue.pop_front().expect("checked above");
            let quote = SourceQuote { ts: quote.ts.min(at), ..quote };
            if self.held.get(&quote.source).is_none_or(|old| quote.ts >= old.ts || old.ts > at) {
                self.held.insert(quote.source.clone(), quote);
            }
        }
    }
}

struct Outcome {
    /// The commands each world sent.
    every_update: usize,
    quiet: usize,
    /// The milliseconds at which the two worlds' indices differed.
    differing: Vec<Millis>,
    /// The milliseconds at which only the quiet world had no index.
    quiet_unpriced_alone: Vec<Millis>,
    /// The milliseconds at which both had an index and the mids differed.
    mids_differing: Vec<Millis>,
    /// The milliseconds at which the every-update world had an index.
    priced: usize,
}

/// Runs the same venues through two engines, one handed every update on arrival and one handed
/// what the feed's rule (`verdict`, with `Pending`) sequences, each sequenced after the same
/// stalls of up to `max_stall` (the sequencer applies nothing during one, then everything queued),
/// and compares the index they give (`compute_index`) at every millisecond.
fn simulate(seed: u64, max_stall: Millis, skew: Millis) -> Outcome {
    const SECONDS: Millis = 120;
    let updates = venue_updates(&mut Rng(seed), SECONDS * 1_000, skew);
    let mut stall_rng = Rng(seed ^ 0xABCDEF);
    let mut stalls = Vec::new();
    let mut at = 2_000;
    while at < SECONDS * 1_000 {
        let length = 1 + stall_rng.below(max_stall as u64) as Millis;
        stalls.push((at, at + length));
        at += length + 500 + stall_rng.below(3_000) as Millis;
    }
    let refresh = Some(refresh_after(1_000, RULES.max_age_ms));
    let (mut every, mut quiet) = (Applied::default(), Applied::default());
    let mut pending: Vec<Pending> = (0..2).map(|_| Pending::default()).collect();
    let mut sent: Vec<Option<(Fixed, Millis)>> = vec![None; 2];
    let mut outcome = Outcome {
        every_update: 0,
        quiet: 0,
        differing: Vec::new(),
        quiet_unpriced_alone: Vec::new(),
        mids_differing: Vec::new(),
        priced: 0,
    };
    let mut arrivals = updates.into_iter().peekable();
    for now in 0..=SECONDS * 1_000 + 2_000 {
        while let Some((_, index, quote)) = arrivals.next_if(|(arrival, ..)| *arrival <= now) {
            every.send(now, quote.clone(), &stalls);
            outcome.every_update += 1;
            pending[index].add(quote, now).unwrap();
        }
        for index in 0..2 {
            let Some((quote, read_stamp)) = pending[index].take() else { continue };
            match verdict(sent[index], &quote, refresh, now) {
                Verdict::Send => {
                    sent[index] = Some(record(sent[index], &quote, now));
                    quiet.send(now, quote, &stalls);
                    outcome.quiet += 1;
                }
                Verdict::Wait(_) => pending[index].defer(quote, read_stamp),
                Verdict::Redundant => {}
            }
        }
        every.apply(now);
        quiet.apply(now);
        let (expected, got) = (compute_index(&every.held, &RULES, now), compute_index(&quiet.held, &RULES, now));
        outcome.priced += usize::from(expected.is_ok());
        if expected != got {
            outcome.differing.push(now);
        }
        match (&expected, &got) {
            (Ok(_), Err(_)) => outcome.quiet_unpriced_alone.push(now),
            (Ok(a), Ok(b)) if a.mid != b.mid => outcome.mids_differing.push(now),
            _ => {}
        }
    }
    outcome
}
