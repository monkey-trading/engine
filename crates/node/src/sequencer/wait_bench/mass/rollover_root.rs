//! What a price waits behind a daily rollover and the state root after it (#925): the core's launch
//! book walked to a millisecond before a daily rollover, in a sequencer with paced checkpoints every
//! 1000 commands, as in production, and its log syncs on their own thread. Prices come 2 ms apart in
//! a market no account holds, the rollover among them, and the run goes on for `WINDOW` after it.
//! With the root worker the checkpoint after the rollover waits for the worker's commit, and with
//! the root computed on the sequencer's thread (the base this case was first run on) it hashes
//! every account the rollover changed inline. Prints the longest wait behind the rollover, the
//! longest in the window after it, the checkpoints written in it and the time to the first, and the
//! latency of account proof reads sent throughout.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use engine_core::command::Command;
use engine_core::types::{AccountId, MarketId};
use tokio::sync::{broadcast, mpsc};

use super::{BAR_US, BEFORE, GUARD, PRICES, SPACING, roll_to, sequencer, stream_market};
use crate::clock::test_clock::TestClock;
use crate::common::launch_book::{Book, Mix, NamedCounts, build, price_command};
use crate::sequencer::wait_bench::{StampClock, report};
use crate::sequencer::{Request, StreamBody};

/// How long the prices go on after the rollover.
const WINDOW: Duration = Duration::from_secs(10);
/// The paced checkpoints' cadence in commands, production's `checkpoint_every`.
const CHECKPOINT_EVERY: u64 = 1_000;
/// The time between two account proof reads.
const READ_EVERY: Duration = Duration::from_millis(3);

/// Quotes every market at its own mark, so no price moves, as a node's feeds keep markets priced.
fn reprice(book: &mut Book) {
    let marks: Vec<(MarketId, String)> =
        book.h.engine.markets().iter().filter_map(|(id, m)| m.mark.map(|mark| (*id, mark.to_string()))).collect();
    for (id, mark) in marks {
        let cmd = price_command(&book.h, id, &mark, book.h.ts);
        book.apply(cmd);
    }
}

/// Runs the case and prints its `WAIT mass_rollover_root` lines.
pub(super) fn run() {
    let accounts = std::env::var("MASS_ACCOUNTS").ok().map_or(super::ACCOUNTS, |n| n.parse().expect("a count"));
    let mut book = build(&Mix::proposal(accounts), NamedCounts { adders: 1, openers: 0, closeouts: 0 });
    let stream = stream_market(&mut book);
    let due = book.h.engine.next_rollover().expect("a rollover is due");
    roll_to(&mut book, due);
    reprice(&mut book);
    let at = due - 1;
    assert_eq!(book.h.ts, at, "the book stands a millisecond before the rollover");
    let prices = PRICES.map(|mid| price_command(&book.h, stream, mid, at));
    let held: Vec<AccountId> = vec![book.named.adders[0].account, book.account(0), book.account(1)];
    let positions: usize = book.h.engine.accounts().values().map(|account| account.positions.len()).sum();
    let engine = std::mem::take(&mut book.h.engine);
    drop(book);
    let accounts = engine.accounts().len();

    let clock = Arc::new(StampClock { inner: TestClock::at(at), stamps: Default::default() });
    let (stream_tx, mut published) = broadcast::channel(65_536);
    let mut seq = sequencer("rollover_root", engine, &clock, stream_tx);
    seq.checkpoint_every = CHECKPOINT_EVERY;
    // The first checkpoint gives the tree its first root, before the run hands it over; a log opens
    // with a command, so a price goes in first.
    seq.sequence(prices[0].clone()).unwrap().unwrap();
    seq.checkpoint().unwrap();
    seq.flush().unwrap();
    let (tx, rx) = mpsc::channel(65_536);
    let runner = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(seq.run(rx)).unwrap();
    });
    // The checkpoints the log gains, with the time each reaches the stream.
    let watcher = std::thread::spawn(move || {
        let mut found: Vec<(Instant, u64)> = Vec::new();
        while let Ok(msg) = published.blocking_recv() {
            if let StreamBody::Checkpoint(cp) = &msg.body {
                found.push((Instant::now(), cp.seq));
            }
        }
        found
    });
    let stop = Arc::new(AtomicBool::new(false));
    // READS-BEGIN
    let reader = {
        let (tx, stop) = (tx.clone(), stop.clone());
        std::thread::spawn(move || {
            let (mut times, mut empty) = (Vec::new(), 0usize);
            let mut next = 0usize;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(READ_EVERY);
                let (reply, answer) = tokio::sync::oneshot::channel();
                let began = Instant::now();
                let account = held[next % held.len()];
                next += 1;
                if tx.blocking_send(Request::AccountProof { account, reply }).is_err() {
                    break;
                }
                match answer.blocking_recv() {
                    Ok(proof) => {
                        empty += usize::from(proof.is_none());
                        times.push(began.elapsed().as_micros());
                    }
                    Err(_) => break,
                }
            }
            (times, empty)
        })
    };
    // READS-END

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
    let began = Instant::now();
    while stamped() < sent.len() {
        assert!(began.elapsed() < GUARD, "the prices before the rollover were not all stamped");
        std::thread::sleep(Duration::from_micros(100));
    }
    clock.inner.advance(Duration::from_millis(1));
    let event_at = sent.len();
    submit(Command::Tick, &mut sent);
    let event_sent = sent[event_at];
    while event_sent.elapsed() < WINDOW {
        submit(prices[sent.len() % 2].clone(), &mut sent);
    }
    let window_end = Instant::now();
    // The queue behind the last price is empty once its stamp is taken.
    let began = Instant::now();
    while stamped() < sent.len() && began.elapsed() < GUARD {
        std::thread::sleep(Duration::from_micros(100));
    }
    stop.store(true, Ordering::Relaxed);
    // READS-BEGIN
    let (mut reads, empty) = reader.join().unwrap();
    // READS-END
    drop(tx);
    runner.join().unwrap();
    let checkpoints = watcher.join().unwrap();

    let all_stamps = clock.stamps.lock().unwrap();
    let stamps = &all_stamps[base..];
    // Each price takes the first stamp at or after its send; a stamp another request took lands
    // between sends and is skipped.
    let mut next = 0;
    let stamp_of = |sent_at: Instant, next: &mut usize| {
        while stamps[*next] < sent_at {
            *next += 1;
        }
        let stamp = stamps[*next];
        *next += 1;
        stamp
    };
    let waits: Vec<u128> =
        sent.iter().map(|sent_at| stamp_of(*sent_at, &mut next).duration_since(*sent_at).as_micros()).collect();
    let (event_wait, extra_stamps) = (waits[event_at], stamps.len() - sent.len());
    let mut at_stamp = 0;
    let stamped_at: Vec<Instant> = sent.iter().map(|sent_at| stamp_of(*sent_at, &mut at_stamp)).collect();
    // The prices behind the rollover: to the first stamped before the next was sent.
    let last = sent.len() - 1;
    let end = (event_at + 1..last).find(|i| stamped_at[*i] < sent[i + 1]).unwrap_or(last);
    let behind = waits[event_at + 1..=end].to_vec();
    let longest_behind = *behind.iter().max().expect("a price behind the rollover");
    let in_window = waits[event_at + 1..].to_vec();
    let longest_window = *in_window.iter().max().expect("prices after the rollover");
    // What the rollover command itself held the thread for, and the longest wait of a price sent after it
    // was done: a checkpoint's root work shows there, the command's own time does not.
    let held = stamped_at[event_at + 1].duration_since(stamped_at[event_at]).as_micros();
    let done = stamped_at[event_at + 1];
    let after_done: Vec<u128> = (event_at + 1..sent.len()).filter(|i| sent[*i] >= done).map(|i| waits[i]).collect();
    let longest_after_done = after_done.iter().copied().max().unwrap_or(0);
    let before_longest = *waits[..event_at].iter().max().expect("prices before");
    let first = checkpoints.iter().find(|(when, _)| *when >= event_sent);
    let written = checkpoints.iter().filter(|(when, _)| *when >= event_sent && *when <= window_end).count();
    let to_first = first.map_or(0, |(when, _)| when.duration_since(event_sent).as_micros());
    println!(
        "WAIT mass_rollover_root accounts={accounts} positions={positions} prices={} stamps_beyond_commands={extra_stamps} \
         rollover_wait_us={event_wait} rollover_held_us={held} longest_after_rollover_done_us={longest_after_done} \
         longest_wait_behind_us={longest_behind} prices_behind={} \
         longest_wait_in_{}s_us={longest_window} longest_before_us={before_longest} checkpoints_in_window={written} \
         checkpoints_total={} first_checkpoint_after_us={to_first}",
        sent.len(),
        behind.len(),
        WINDOW.as_secs(),
        checkpoints.len()
    );
    report("mass_rollover_root_prices_before", waits[..event_at].to_vec());
    report("mass_rollover_root_prices_behind", behind);
    report("mass_rollover_root_prices_in_window", in_window);
    report("mass_rollover_root_prices_after_rollover_done", after_done);
    // READS-BEGIN
    println!("WAIT mass_rollover_root_account_proofs empty_answers={empty}");
    report("mass_rollover_root_account_proof_reads", std::mem::take(&mut reads));
    // READS-END
    let verdict = if longest_window <= BAR_US { "within" } else { "OVER" };
    println!("WAIT bar mass_rollover_root longest_wait_us={longest_window} bar_us={BAR_US} {verdict}");
    assert!(first.is_some(), "a checkpoint follows the rollover");
    assert!(longest_window < GUARD.as_micros(), "a price waited {longest_window}us after the rollover");
}
