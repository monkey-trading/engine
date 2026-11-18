//! What bursts of proof reads cost the price path (#925): the launch book (100k accounts) in a
//! sequencer with paced checkpoints and its tree on the root worker, prices 2 ms apart through the
//! real `run` loop, and every second a burst of `BURST` reads at once (production's
//! `api.max_connections`), alternating account and exit proofs. The reads reach the worker by way of
//! the sequencer's request channel (`forward_read`). Prints the cost of one forward on the
//! sequencer's thread, then the price waits without and with bursts, and the reads' answer latency.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine_core::types::AccountId;
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{PRICES, SPACING, sequencer, stream_market};
use crate::clock::test_clock::TestClock;
use crate::common::launch_book::{Mix, NamedCounts, build, price_command};
use crate::sequencer::wait_bench::{StampClock, report, time};
use crate::sequencer::{Request, Sequencer};

/// Reads in a burst.
const BURST: usize = 512;
/// How long each phase sends prices, without and with bursts.
const PHASE: Duration = Duration::from_secs(10);
const CHECKPOINT_EVERY: u64 = 1_000;

/// A read of `kind` (0 account, 1 exit) for `account`, and the thread-side wait for its answer.
fn read(kind: usize, account: AccountId) -> (Request, Box<dyn FnOnce() -> bool + Send>) {
    if kind == 0 {
        let (reply, answer) = oneshot::channel();
        (Request::AccountProof { account, reply }, Box::new(move || answer.blocking_recv().is_ok()))
    } else {
        let (reply, answer) = oneshot::channel();
        (Request::ExitProof { account, reply }, Box::new(move || answer.blocking_recv().is_ok()))
    }
}

/// The cost of one forward on the sequencer's thread, per kind of read, with every answer taken.
fn forward_cost(seq: &mut Sequencer, accounts: &[AccountId]) {
    seq.start_root_worker().unwrap();
    for (kind, name) in [(0, "account_proof"), (1, "exit_proof")] {
        let mut times = Vec::new();
        let mut waits = Vec::new();
        for round in 0..4 {
            for index in 0..BURST {
                let (request, wait) = read(kind, accounts[(round * BURST + index) % accounts.len()]);
                times.push(time(|| seq.forward_read(request).unwrap()).0);
                waits.push(wait);
            }
            for wait in waits.drain(..) {
                assert!(wait(), "the worker answers");
            }
        }
        let total: u128 = times.iter().sum();
        println!("WAIT proof_burst_forward_{name} total_per_burst_us={}", total * BURST as u128 / times.len() as u128);
        report(&format!("proof_burst_forward_{name}_one_read"), times);
    }
}

/// Runs the case.
pub(super) fn run() {
    let accounts = std::env::var("MASS_ACCOUNTS").ok().map_or(super::ACCOUNTS, |n| n.parse().expect("a count"));
    let mut book = build(&Mix::proposal(accounts), NamedCounts { adders: 1, openers: 0, closeouts: 0 });
    let stream = stream_market(&mut book);
    let at = book.h.ts;
    let prices = PRICES.map(|mid| price_command(&book.h, stream, mid, at));
    let mut held: Vec<AccountId> = vec![book.named.adders[0].account];
    held.extend((0..64).map(|i| book.account(i)));
    let engine = std::mem::take(&mut book.h.engine);
    drop(book);
    let clock = Arc::new(StampClock { inner: TestClock::at(at), stamps: Default::default() });
    let (stream_tx, _published) = broadcast::channel(65_536);
    let mut seq = sequencer("proof_burst", engine, &clock, stream_tx);
    seq.checkpoint_every = CHECKPOINT_EVERY;
    seq.sequence(prices[0].clone()).unwrap().unwrap();
    seq.checkpoint().unwrap();
    seq.flush().unwrap();
    forward_cost(&mut seq, &held);
    let (tx, rx) = mpsc::channel(65_536);
    let runner = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(seq.run(rx)).unwrap();
    });
    let latencies = Arc::new(Mutex::new(Vec::<u128>::new()));
    let (mut sent, mut bursts, mut helpers): (Vec<Instant>, Vec<(Instant, Instant)>, Vec<_>) = <_>::default();
    let mut phase_end: Vec<usize> = Vec::new();
    let base = clock.stamps.lock().unwrap().len();
    for phase in 0..2 {
        let began = Instant::now();
        let mut next_burst = Duration::from_millis(500);
        let mut count = 0usize;
        while began.elapsed() < PHASE {
            std::thread::sleep(SPACING);
            sent.push(Instant::now());
            tx.blocking_send(Request::Submit { cmd: prices[sent.len() % 2].clone(), reply: None }).unwrap();
            if phase == 1 && began.elapsed() >= next_burst {
                next_burst += Duration::from_secs(1);
                let (tx, latencies, held, kind) = (tx.clone(), latencies.clone(), held.clone(), count % 2);
                count += 1;
                let from = Instant::now();
                helpers.push(std::thread::spawn(move || {
                    let waits: Vec<_> = (0..BURST)
                        .map(|i| {
                            let (request, wait) = read(kind, held[i % held.len()]);
                            tx.blocking_send(request).unwrap();
                            wait
                        })
                        .collect();
                    let queued = Instant::now();
                    for wait in waits {
                        assert!(wait(), "answered");
                        latencies.lock().unwrap().push(from.elapsed().as_micros());
                    }
                    (from, queued)
                }));
            }
        }
        // The prices of the phase end at this index.
        phase_end.push(sent.len());
    }
    for helper in helpers {
        bursts.push(helper.join().unwrap());
    }
    while clock.stamps.lock().unwrap().len() - base < sent.len() {
        std::thread::sleep(Duration::from_micros(100));
    }
    drop(tx);
    runner.join().unwrap();
    let all = clock.stamps.lock().unwrap();
    let stamps = &all[base..];
    let mut next = 0;
    let waits: Vec<u128> = sent
        .iter()
        .map(|sent_at| {
            while stamps[next] < *sent_at {
                next += 1;
            }
            next += 1;
            stamps[next - 1].duration_since(*sent_at).as_micros()
        })
        .collect();
    let split = phase_end[0];
    report("proof_burst_prices_without_bursts", waits[..split].to_vec());
    report("proof_burst_prices_with_bursts", waits[split..].to_vec());
    let queueing: Vec<u128> = bursts.iter().map(|(from, queued)| queued.duration_since(*from).as_micros()).collect();
    println!("WAIT proof_burst bursts={} reads={} queueing_each_us={queueing:?}", bursts.len(), BURST * bursts.len());
    report("proof_burst_read_answer_latency", latencies.lock().unwrap().clone());
}
