//! What a price waits behind a mass close-out on the node's sequencer (#907): the start of WTI's
//! weekend gap, with the Friday rollover at the same instant, and a BTC-USD price a tenth down, each
//! closing out a fifth of the market's holders on the core's launch book (`common::launch_book`: 100k
//! accounts at the latest protocol on the production genesis; `MASS_ACCOUNTS` runs a smaller one). The
//! real `run` loop sequences a price every 2 ms with the event among them, its log syncs on their own
//! thread and the stats record following, and each price's wait from entering the channel to its stamp
//! is timed as `price_wait_budget` times it (`StampClock`). Prints the event and the longest wait
//! behind it as `WAIT mass_<case> ...`, the waits before and behind it, and the longest against #904's
//! 500 ms ceiling on one command as `WAIT bar ...`, `OVER` past it; a case fails only past a 10 s guard.
//! Run in a quiet CPU window:
//! `cargo test -p engine-node --lib sequencer::wait_bench::mass -- --ignored --nocapture --test-threads=1`.
//!
//! The prices are in a copy of BTC-USD listed for them, which no account holds, as no account holds
//! `price_wait_budget`'s market: on this book a price visits every position its market's holders hold,
//! 52 to 133 ms a price at 100k (#919), so a held market's prices 2 ms apart would queue without end
//! with no event among them.
//!
//! The book is assembled, and walked to its event, on the core's harness, as the core's mass cases
//! walk it: the weekend case through the daily rollovers to a millisecond before the gap, each with its
//! rates published first, the Friday's included, and the crash case 70 s on with every other market
//! priced. Its engine then replaces that of a sequencer recovered from an empty log, which takes the
//! engine's next seq and last stamp as recovery takes a replayed engine's. The log so holds only what
//! the run sequences, which no replay could check, and no checkpoint is written before the drain's.

use std::sync::Arc;
use std::time::{Duration, Instant};

use engine_core::Engine;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::types::{MarketId, Millis};
use tokio::sync::{broadcast, mpsc, watch};

mod rollover_root;

use crate::clock::test_clock::TestClock;
use crate::common::launch_book::{Book, Mix, NamedCounts, Weak, WeakKind, benchmark_batch, build, price_command};
use crate::common::{BTC, f};
use crate::log::Manifest;
use crate::sequencer::wait_bench::{StampClock, report};
use crate::sequencer::{Request, Sequencer, SequencerConfig, StreamBody, StreamMsg};
use crate::tee::EnclaveKey;
use crate::test_support::{sample_around, scratch_dir, sequencer_config, stats};

const ACCOUNTS: usize = 100_000;
const WTI: MarketId = 32;
/// The share of the market's holders, in per cent, that the event closes out.
const SHARE: u64 = 20;
/// The price BTC-USD falls to, a tenth below the book's mark.
const CRASH_MID: &str = "90";
/// The stream's two prices, sent in turn so each moves the mark.
const PRICES: [&str; 2] = ["100", "100.01"];
/// The time between two prices, as in `price_wait_budget`.
const SPACING: Duration = Duration::from_millis(2);
/// Prices before the event, and the fewest after it: the run goes on until every price behind it is
/// stamped.
const BEFORE: usize = 250;
const AFTER: usize = 250;
/// #904's ceiling on one command, which the longest wait behind it shows.
const BAR_US: u128 = 500_000;
/// What a case is held to, well above the bar, so a regression fails while the book misses the bar.
const GUARD: Duration = Duration::from_secs(10);

/// A mass event on a walked book: the market whose holders it closes out, the time the prices are
/// stamped at, and the stream's two prices, sent in turn.
pub(super) struct Case {
    pub(super) name: &'static str,
    pub(super) market: MarketId,
    pub(super) at: Millis,
    /// Whether the clock moves a millisecond on just before the event, which is then the first
    /// command stamped there.
    pub(super) step: bool,
    pub(super) event: Command,
    pub(super) prices: [Command; 2],
}

/// The book the event ran on, and the accounts it closed out.
pub(super) struct Outcome {
    pub(super) weak: usize,
    pub(super) closed: usize,
    /// The positions the event's command financed, the fills it made, and the positions the book held.
    pub(super) financing: usize,
    pub(super) fills: usize,
    pub(super) positions: usize,
    /// The events the event's command logged.
    pub(super) emitted: Vec<Event>,
}

/// The start of WTI's weekend gap, with a fifth of its holders below their weekend margin.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_weekend_gap_start() {
    let mut book = book(WTI, WeakKind::BelowWeekendMargin);
    let stream = stream_market(&mut book);
    let gap = book.h.engine.markets()[&WTI].weekend_window().expect("WTI has a weekend gap");
    roll_to(&mut book, gap.close);
    assert_eq!(book.h.engine.next_rollover(), Some(gap.close), "the Friday rollover falls at the gap's start");
    let at = gap.close - 1;
    let prices = PRICES.map(|mid| price_command(&book.h, stream, mid, at));
    let case = Case { name: "weekend_gap_start_20pct", market: WTI, at, step: true, event: Command::Tick, prices };
    let Outcome { weak, closed, financing, fills, positions, .. } = sequence(book, case);
    assert!(
        financing == positions || financing + fills == positions,
        "{financing} financed, {fills} filled, of {positions}"
    );
    assert_eq!(closed, weak, "the accounts below the weekend margin, and no others, are closed out");
}

/// One BTC-USD price a tenth down that closes out a fifth of its holders.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_crash() {
    let mut book = book(BTC, WeakKind::FallsBelowAt(f(CRASH_MID)));
    let stream = stream_market(&mut book);
    book.h.advance(70_000);
    let others: Vec<MarketId> = book.h.engine.markets().keys().copied().filter(|id| *id != BTC).collect();
    for id in others {
        let cmd = price_command(&book.h, id, "100", book.h.ts);
        let events = book.apply(cmd);
        assert!(!events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "market {id}: {events:?}");
    }
    let at = book.h.ts;
    let prices = PRICES.map(|mid| price_command(&book.h, stream, mid, at));
    let event = price_command(&book.h, BTC, CRASH_MID, at);
    let case = Case { name: "crash_closes_out_20pct", market: BTC, at, step: false, event, prices };
    let Outcome { weak, closed, .. } = sequence(book, case);
    assert_eq!(closed, weak, "the accounts below the close-out level, and no others, close");
}

/// A daily rollover with paced checkpoints running, and the root of the checkpoint after it (#925).
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_rollover_root() {
    rollover_root::run();
}

/// The launch book with `SHARE` per cent of `market`'s holders set as `kind` says.
fn book(market: MarketId, kind: WeakKind) -> Book {
    let mix = Mix { weak: Some(Weak { market, share: SHARE, kind }), ..Mix::proposal(accounts()) };
    build(&mix, NamedCounts { adders: 1, openers: 0, closeouts: 0 })
}

/// The proposal's book size; `MASS_ACCOUNTS` runs a smaller one.
pub(super) fn accounts() -> usize {
    std::env::var("MASS_ACCOUNTS").ok().map_or(ACCOUNTS, |n| n.parse().expect("a count"))
}

/// Lists a copy of BTC-USD, which no account holds, for the stream's prices. A listing after genesis
/// needs a close-spread ceiling adopted seven days before it (`close_spread`), so BTC-USD's ceiling is
/// written into the engine's encoding under the copy's id first, as the book writes its accounts in.
pub(super) fn stream_market(book: &mut Book) -> MarketId {
    let id = book.h.engine.markets().keys().max().expect("listed markets") + 1;
    let ceiling = book.h.engine.close_spread_ceilings().expect("a ceiling registry")[&BTC];
    let mut text = serde_json::to_string(&book.h.engine).expect("encoding");
    let registry = "\"close_spread_ceilings\":{";
    assert_eq!(text.matches(registry).count(), 1, "one ceiling registry");
    let at = text.find(registry).expect("the ceiling registry") + registry.len();
    text.insert_str(at, &format!("\"{id}\":{ceiling},"));
    book.h.engine = serde_json::from_str(&text).expect("the book decodes");
    drop(text);
    book.h.engine.restore_indexes();
    let spec = MarketSpec { id, symbol: "STREAM-USD".into(), ..book.h.engine.markets()[&BTC].spec.clone() };
    let events = book.operate(AdminCommand::UpsertMarket(spec));
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    id
}

/// Walks the book's clock to a millisecond before `to` through the daily rollovers before it, each with
/// its rates published first; a rollover at `to` itself gets its rates there and is left for the
/// command at `to`.
pub(super) fn roll_to(book: &mut Book, to: Millis) {
    while let Some(due) = book.h.engine.next_rollover().filter(|due| *due <= to) {
        book.h.advance_to(due - 1);
        let batch = benchmark_batch(&book.h);
        let events = book.operate(batch);
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
        if due == to {
            return;
        }
        book.h.advance_to(due);
        book.apply(Command::Tick);
    }
    book.h.advance_to(to - 1);
}

/// A sequencer recovered from an empty log, holding `engine` as recovery holds a replayed one, with
/// trusted time around `clock`, the stats record following, and its log syncs on their own thread.
fn sequencer(
    name: &str,
    engine: Engine,
    clock: &Arc<StampClock>,
    stream: broadcast::Sender<Arc<StreamMsg>>,
) -> Sequencer {
    let dir = scratch_dir(&format!("mass-{name}"));
    let (withdrawals, signed) = mpsc::unbounded_channel();
    std::mem::forget(signed);
    let cfg = SequencerConfig {
        clock: clock.clone(),
        checkpoint_every: u64::MAX,
        segment_commands: u64::MAX,
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), stream, withdrawals).unwrap();
    seq.next_seq = engine.last_seq().map_or(0, |seq| seq + 1);
    seq.last_ts = engine.now();
    seq.last_checkpoint = engine.last_seq();
    seq.engine = engine;
    seq.track_stats(stats());
    seq.trust(sample_around(&clock.inner)).unwrap();
    seq.pipeline_log_syncs().unwrap();
    seq
}

/// Runs `case` on `book`'s engine through the sequencer's `run` loop, prints the waits, and holds the
/// longest to the guard.
pub(super) fn sequence(mut book: Book, case: Case) -> Outcome {
    let Case { name, market, at, step, event, prices } = case;
    let (holders, weak, positions) = (
        book.holders.get(&market).copied().unwrap_or(0),
        book.weak_accounts,
        book.h.engine.accounts().values().map(|account| account.positions.len()).sum(),
    );
    let engine = std::mem::take(&mut book.h.engine);
    drop(book);
    assert!(engine.now() <= at, "the book's clock is past the stream's");
    let accounts = engine.accounts().len();
    let clock = Arc::new(StampClock { inner: TestClock::at(at), stamps: Default::default() });
    let (stream, mut published) = broadcast::channel(65_536);
    let seq = sequencer(name, engine, &clock, stream);
    let (tx, rx) = mpsc::channel(65_536);
    let runner = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(seq.run(rx)).unwrap();
    });
    let base = clock.stamps.lock().unwrap().len();
    let stamped = || clock.stamps.lock().unwrap().len() - base;
    let mut sent: Vec<Instant> = Vec::new();
    let submit = |cmd: Command, sent: &mut Vec<Instant>| {
        std::thread::sleep(SPACING);
        sent.push(Instant::now());
        tx.blocking_send(Request::Submit { cmd, reply: None }).unwrap();
    };
    for index in 0..BEFORE {
        submit(prices[index % 2].clone(), &mut sent);
    }
    // The event goes in once every price before it is stamped, so a stepped clock stamps it first.
    let began = Instant::now();
    while stamped() < sent.len() {
        assert!(began.elapsed() < GUARD, "the prices before the event were not all stamped");
        std::thread::sleep(Duration::from_micros(100));
    }
    if step {
        clock.inner.advance(Duration::from_millis(1));
    }
    let event_at = sent.len();
    submit(event.clone(), &mut sent);
    let began = Instant::now();
    while (sent.len() - event_at <= AFTER || stamped() < sent.len()) && began.elapsed() < GUARD {
        submit(prices[sent.len() % 2].clone(), &mut sent);
    }
    drop(tx);
    // The runner's exit drains every submitted command, so each is stamped.
    runner.join().unwrap();
    let stamps = clock.stamps.lock().unwrap()[base..].to_vec();
    // One stamp per command, in the channel's order.
    assert_eq!(stamps.len(), sent.len(), "every command was stamped once");
    let close_outs = |events: &[Event]| events.iter().filter(|e| matches!(e, Event::CloseOut { .. })).count();
    let (mut commands, mut all_closed, mut emitted) = (0, 0, None);
    while let Ok(msg) = published.try_recv() {
        if let StreamBody::Command(entry) = &msg.body {
            commands += 1;
            let events = entry.events.as_deref().unwrap_or_default();
            all_closed += close_outs(events);
            if entry.sequenced.cmd == event {
                emitted = Some(events.to_vec());
            }
        }
    }
    assert_eq!(commands, sent.len(), "every command was sequenced");
    let emitted = emitted.expect("the event was sequenced");
    let closed = close_outs(&emitted);

    let waits: Vec<u128> =
        sent.iter().zip(&stamps).map(|(sent_at, stamp)| stamp.duration_since(*sent_at).as_micros()).collect();
    // The prices behind the event: from the one after it to the first the sequencer stamped before the
    // next was sent, which found the queue empty.
    let last = sent.len() - 1;
    let end = (event_at + 1..last).find(|i| stamps[*i] < sent[i + 1]).unwrap_or(last);
    let behind = waits[event_at + 1..=end].to_vec();
    let longest = *behind.iter().max().expect("a price behind the event");
    let held = stamps[event_at + 1].duration_since(stamps[event_at]).as_micros();
    let fills = emitted.iter().filter(|e| matches!(e, Event::Fill(_))).count();
    println!(
        "WAIT mass_{name} accounts={accounts} holders={holders} below={weak} closed_out={closed} \
         closed_out_later={} fills={fills} events={} held_us={held} longest_wait_us={longest} prices_behind={}",
        all_closed - closed,
        emitted.len(),
        behind.len()
    );
    report(&format!("mass_{name}_prices_before"), waits[..event_at].to_vec());
    report(&format!("mass_{name}_prices_behind"), behind);
    let verdict = if longest <= BAR_US { "within" } else { "OVER" };
    println!("WAIT bar mass_{name} longest_wait_us={longest} bar_us={BAR_US} {verdict}");
    assert!(longest < GUARD.as_micros(), "a price waited {longest}us behind {name}, past the guard");
    // Each position financed: an event before `V80`, and from it a row its rollover's aggregate covers.
    let financing = emitted
        .iter()
        .map(|e| match e {
            Event::Financing { .. } => 1,
            Event::FinancingApplied { positions, .. } => usize::try_from(*positions).expect("a count"),
            _ => 0,
        })
        .sum();
    Outcome { weak, closed, financing, fills, positions, emitted }
}
