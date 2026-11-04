//! What a price command can wait behind on the sequencer's thread, per occurrence, at 100k accounts:
//! the command's own stamp, the batch's log write and sync, the reads answered after it, a
//! checkpoint's root, signature and captures, and the requests the sequencer answers itself. Run in
//! a quiet CPU window:
//! `cargo test -p engine-node --lib sequencer::wait_bench -- --ignored --nocapture --test-threads=1`.

use std::sync::Arc;
use std::time::Instant;

use engine_core::command::Command;
use engine_core::price::SourceQuote;
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};
use tokio::sync::mpsc::Receiver;
use tokio::sync::{broadcast, mpsc, watch};

use crate::checkpoints::{ApprovalId, ApprovalStatus, Health, LifecycleEvidence, Registry, Work};
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::log::Manifest;
use crate::sequencer::pipeline::Batch;
use crate::sequencer::{Sequencer, SequencerConfig};
use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, empty_custody_frontier, sample_around, scratch_dir, sequencer_config};

const ACCOUNTS: u32 = 100_000;

/// A gated sequencer holding `ACCOUNTS` funded accounts, its checkpoints final.
struct Rig {
    seq: Sequencer,
    queued: Receiver<Work>,
    clock: Arc<TestClock>,
    round: u32,
}

/// The `index`th funded account.
fn account(index: u32) -> AccountId {
    let mut bytes = [1; 20];
    bytes[16..].copy_from_slice(&index.to_be_bytes());
    AccountId(bytes)
}

impl Rig {
    fn new(name: &str) -> Self {
        let dir = scratch_dir(name);
        let clock = Arc::new(TestClock::default());
        let (sink, queued) = mpsc::channel(1024);
        let (withdrawals, signed) = mpsc::unbounded_channel();
        std::mem::forget(signed);
        let cfg = SequencerConfig {
            clock: clock.clone(),
            checkpoints: Some(sink),
            checkpoint_every: u64::MAX,
            segment_commands: u64::MAX,
            checkpoint_registry: Some(Registry::default()),
            approval_registry: Some(Default::default()),
            ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
        };
        let mut seq =
            Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(4096).0, withdrawals).unwrap();
        seq.track_stats(crate::test_support::stats());
        seq.activate_legacy_protocol().unwrap();
        seq.genesis(&dev_config().genesis).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        seq.sequence(empty_custody_frontier()).unwrap();
        seq.checkpoint().unwrap();
        seq.checkpoint_status(seq.last_seq(), Some(Health::Healthy)).unwrap();
        let mut rig = Self { seq, queued, clock, round: 0 };
        rig.touch(ACCOUNTS);
        rig.checkpoint_final();
        rig
    }

    /// Moves the log's data sync to its own thread, as the node runs.
    fn pipelined(&mut self) {
        self.seq.pipeline_log_syncs().unwrap();
    }

    /// Credits the first `count` accounts, so a root hashes that many changed leaves.
    fn touch(&mut self, count: u32) {
        self.round += 1;
        for index in 0..count {
            let chain_ref = format!("wait-{}-{index}", self.round);
            let cmd = Command::Deposit { account: account(index), amount: Fixed::from_int(10), chain_ref };
            self.seq.sequence(cmd).unwrap().unwrap();
            if index % 1024 == 1023 {
                self.seq.flush().unwrap();
            }
        }
        self.seq.flush().unwrap();
    }

    /// Signs a checkpoint of the state so far and reports it final.
    fn checkpoint_final(&mut self) -> crate::log::Checkpoint {
        self.seq.checkpoint().unwrap();
        self.seq.flush().unwrap();
        let checkpoint = std::iter::from_fn(|| self.queued.try_recv().ok()).last().expect("a checkpoint").checkpoint;
        self.seq.checkpoint_status(Some(checkpoint.seq), Some(Health::Healthy)).unwrap();
        self.seq.flush().unwrap();
        checkpoint
    }

    /// A two-venue price for market 1, moving every call so the mark changes.
    fn price(&self, step: u32) -> Command {
        let mid = Fixed::from_int(100_000 + i64::from(step % 2));
        let ts = self.clock.as_ref().now_ms();
        let quotes =
            ["venue-a", "venue-b"].map(|source| SourceQuote { source: source.into(), mid, ts, contract: None });
        Command::Prices { market: 1, quotes: quotes.to_vec() }
    }
}

/// Prints one measured item: its samples' p50, p99 and worst, in microseconds.
fn report(name: &str, mut times: Vec<u128>) {
    times.sort_unstable();
    let n = times.len();
    println!(
        "WAIT {name} samples={n} p50_us={} p99_us={} max_us={}",
        times[n / 2],
        times[(n * 99 / 100).min(n - 1)],
        times[n - 1]
    );
}

/// Runs `f`, returning microseconds it took and its result.
fn time<T>(f: impl FnOnce() -> T) -> (u128, T) {
    let began = Instant::now();
    let out = f();
    (began.elapsed().as_micros(), out)
}

/// The command's own stamp, and the batch tail of one command: the log's write and data sync, and
/// the same with the directory entry (a batch that signs a withdrawal).
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn stamp_and_batch_tail() {
    let mut rig = Rig::new("wait-stamp");
    let (mut stamps, mut tails, mut dir_syncs, mut staged): (Vec<u128>, Vec<u128>, Vec<u128>, Vec<u128>) =
        <_>::default();
    let (mut marks, mut house): (Vec<u128>, Vec<u128>) = <_>::default();
    for step in 0..2000 {
        let cmd = rig.price(step);
        stamps.push(time(|| rig.seq.sequence(cmd).unwrap().unwrap()).0);
        marks.push(
            time(|| {
                let reports = rig.seq.marks.take_pending();
                rig.seq.marks.release(reports);
            })
            .0,
        );
        let (reply, _read) = tokio::sync::oneshot::channel();
        rig.seq.house.wait(reply);
        house.push(time(|| rig.seq.house.take_answer(&rig.seq.engine, rig.seq.progress.params_seq)).0);
        tails.push(time(|| rig.seq.flush().unwrap()).0);
        let cmd = rig.price(step + 1);
        rig.seq.sequence(cmd).unwrap().unwrap();
        dir_syncs.push(time(|| rig.seq.log.sync().unwrap()).0);
    }
    rig.pipelined();
    for step in 0..2000 {
        // What the sequencer's thread pays for a batch once the sync runs on its own thread.
        let cmd = rig.price(step);
        rig.seq.sequence(cmd).unwrap().unwrap();
        staged.push(
            time(|| {
                let release = rig.seq.stage(Batch::default()).unwrap();
                rig.seq.submit(release).unwrap();
            })
            .0,
        );
        rig.seq.settle().unwrap();
    }
    report("command_stamp_stats_marks_history_log_append", stamps);
    report("release_marks", marks);
    report("house_answer", house);
    report("batch_tail_flush_data_sync", tails);
    report("batch_tail_sync_with_directory", dir_syncs);
    report("batch_tail_pipelined_stage_and_submit", staged);
}

/// The read closures the API runs on the thread, over the 100k-account engine.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn reads() {
    let rig = Rig::new("wait-reads");
    let stats = crate::test_support::stats();
    let id = account(7);
    let engine: &Engine = &rig.seq.engine;
    let run = |name: &str, read: &dyn Fn(&Engine)| {
        let times = (0..500).map(|_| time(|| read(engine)).0).collect();
        report(&format!("read_{name}"), times);
    };
    run("markets", &|e| drop(e.markets().values().map(|m| (m.spec.clone(), m.open, m.mark)).collect::<Vec<_>>()));
    run("house_cash_nav_exposure", &|e| {
        _ = std::hint::black_box((e.house_cash(), e.house_nav(), e.house_gross_exposure()))
    });
    run("house_figures", &|e| drop(stats.house(e)));
    run("account", &|e| {
        let acct = e.accounts().get(&id).unwrap();
        drop((e.equity_and_margin(acct, None), acct.positions.clone(), acct.orders.clone()));
    });
    run("nonce", &|e| _ = std::hint::black_box(e.next_request_nonce(id)));
    run("account_proof", &|e| drop(e.account_proof(id)));
    run("exit_proof", &|e| drop(e.exit_proof(id)));
    run("withdrawal_queue_page", &|e| drop(e.withdrawal_queue_page(Some(id), None, 100)));
    run("benchmark_state", &|e| drop(e.benchmark_state()));
    run("distributable_excess", &|e| _ = std::hint::black_box(e.distributable_excess()));
}

/// A checkpoint's parts, for a root over `changed` leaves: the root, the signature, the whole call
/// (which also syncs the log and its directory), and the engine's encoding a snapshot captures.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn checkpoint_parts() {
    let mut rig = Rig::new("wait-checkpoint");
    for pipelined in [false, true] {
        if pipelined {
            rig.pipelined();
        }
        let mode = if pipelined { "pipelined" } else { "inline" };
        for changed in [10, 1_000, 10_000, 100_000] {
            let (mut roots, mut whole) = (Vec::new(), Vec::new());
            for _ in 0..5 {
                rig.touch(changed);
                roots.push(time(|| rig.seq.engine.state_root()).0);
                rig.touch(changed);
                whole.push(time(|| rig.seq.checkpoint().unwrap()).0);
                rig.seq.flush().unwrap();
                rig.queued.try_recv().ok();
                rig.seq.checkpoint_status(rig.seq.last_seq(), Some(Health::Healthy)).unwrap();
            }
            report(&format!("checkpoint_state_root_changed={changed}"), roots);
            report(&format!("checkpoint_whole_{mode}_changed={changed}"), whole);
        }
    }
    let root = [7; 32];
    let signs = (0..200)
        .map(|_| {
            time(|| rig.seq.key.sign_checkpoint_v2(31337, &[0; 20], 1, &root, rig.seq.gate.as_ref().unwrap().parent)).0
        })
        .collect();
    report("checkpoint_signature", signs);
    let (encoding, bytes) = time(|| serde_json::to_string(&rig.seq.engine).unwrap().len());
    println!("WAIT snapshot_engine_encoding samples=1 us={encoding} bytes={bytes}");
}

/// The requests the sequencer answers itself, and a release of a page of signed authorizations.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn requests() {
    let mut rig = Rig::new("wait-requests");
    for index in 0..1_000 {
        let withdraw = Command::Withdraw { account: account(index), amount: Fixed::ONE, auth: None };
        rig.seq.sequence(withdraw).unwrap().unwrap();
        rig.queued.try_recv().ok();
    }
    let checkpoint = rig.checkpoint_final();
    let id = account(5);
    let payouts = (0..500).map(|_| time(|| rig.seq.account_payouts(id, None, 100)).0).collect();
    report("request_account_payouts", payouts);
    let claims = (0..500).map(|_| time(|| rig.seq.engine.account_proof(id)).0).collect();
    report("request_claim_proof_primitive", claims);
    let approvals: Vec<ApprovalStatus> = rig
        .seq
        .engine
        .withdrawal_approvals()
        .map(|a| ApprovalStatus {
            approval: ApprovalId { account: a.account, nonce: a.nonce, approved_seq: a.approved_seq },
            claimed: false,
        })
        .collect();
    let evidence = LifecycleEvidence {
        height: 1,
        block_hash: alloy::primitives::B256::repeat_byte(1),
        signer: AccountId(rig.seq.key.address()),
        signer_epoch: 1,
        withdrawals_paused: false,
        total_withdrawn: Fixed::ZERO,
        checkpoint_seq: Some(checkpoint.seq),
        checkpoint_root: Some(checkpoint.root.clone()),
        approvals,
    };
    let (release, ()) =
        time(|| rig.seq.custody_status(Some(checkpoint.seq), Some(Health::Healthy), Some(evidence)).unwrap());
    println!("WAIT release_payout_page samples=1 us={release} approvals_pending=1000");
}

/// A clock that notes when each stamp is taken: the sequencer reads the wall clock once per command,
/// at the point it stamps it.
struct StampClock {
    inner: TestClock,
    stamps: std::sync::Mutex<Vec<Instant>>,
}

impl Clock for StampClock {
    fn now_ms(&self) -> engine_core::types::Millis {
        self.stamps.lock().unwrap().push(Instant::now());
        self.inner.now_ms()
    }

    fn instant(&self) -> Instant {
        self.inner.instant()
    }
}

/// The time from a price entering the channel to its stamp, with the work that used to hold it back
/// going on: proof reads on the sequencer, a checkpoint every `EVERY` prices, and a disk whose every
/// sync stalls 10 ms. Prices come 2 ms apart and the real `run` loop sequences them, in `BATCHES`
/// batches measured separately. A price that waited for a stalled sync would wait out the stall
/// behind every checkpoint (over a tenth of the batch's prices), so a batch's p90 must stay under
/// the stall; a per-price slowdown moves the median on every batch. A scheduler hiccup on a loaded
/// runner lifts wall-clock waits without any work behind them, so the median must hold on every
/// batch while the p90 must hold on at least one; every batch is reported in full.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn price_wait_budget() {
    const EVERY: u64 = 25;
    const PRICES: usize = 600;
    const BATCHES: usize = 3;
    const MEDIAN_US: u128 = 1_000;
    const P90_US: u128 = 5_000;
    const SYNC_STALL: std::time::Duration = std::time::Duration::from_millis(10);
    let mut rig = Rig::new("wait-budget");
    let clock = Arc::new(StampClock { inner: TestClock::default(), stamps: Default::default() });
    rig.seq.clock = clock.clone();
    rig.seq.checkpoint_every = EVERY;
    rig.seq.trust(sample_around(clock.as_ref())).unwrap();
    // A disk that takes `SYNC_STALL` more than it should on every sync: a price that waits for one
    // shows it.
    rig.seq
        .pipeline_with_syncs(|job| {
            std::thread::sleep(SYNC_STALL);
            job.sync()
        })
        .unwrap();
    let mut seq = rig.seq;
    let prices: Vec<Command> = (0..PRICES as u32)
        .map(|step| {
            let mid = Fixed::from_int(100_000 + i64::from(step % 2));
            let quotes = ["venue-a", "venue-b"].map(|source| SourceQuote {
                source: source.into(),
                mid,
                ts: clock.inner.now_ms(),
                contract: None,
            });
            Command::Prices { market: 1, quotes: quotes.to_vec() }
        })
        .collect();
    seq.checkpoint().ok();
    let (tx, rx) = mpsc::channel(65_536);
    let runner = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(seq.run(rx)).unwrap();
    });
    let id = account(9);
    let load = {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            while started.elapsed() < std::time::Duration::from_millis(2 * PRICES as u64 * BATCHES as u64 + 500) {
                let read: crate::sequencer::Query = Box::new(move |engine: &Engine| {
                    drop((engine.account_proof(id), engine.exit_proof(id), engine.distributable_excess()));
                });
                if tx.blocking_send(crate::sequencer::Request::Query(read)).is_err() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })
    };
    let mut batches = Vec::new();
    for _ in 0..BATCHES {
        let before = clock.stamps.lock().unwrap().len();
        let mut sent = Vec::new();
        for cmd in prices.clone() {
            std::thread::sleep(std::time::Duration::from_millis(2));
            sent.push(Instant::now());
            tx.blocking_send(crate::sequencer::Request::Submit { cmd, reply: None }).unwrap();
            // Evidence that the last checkpoint is final lets the next one sign.
            if (sent.len() as u64).is_multiple_of(EVERY) {
                let status = crate::sequencer::Request::CheckpointStatus {
                    posted: Some(u64::MAX / 2),
                    latest: None,
                    health: Some(Health::Healthy),
                    lifecycle: None,
                };
                let _ = tx.blocking_send(status);
            }
        }
        batches.push((before, sent));
    }
    load.join().unwrap();
    drop(tx);
    // The runner's exit drains every submitted command, so each batch's prices are all stamped.
    runner.join().unwrap();
    let all_stamps = clock.stamps.lock().unwrap();
    let stats: Vec<(u128, u128)> = batches
        .iter()
        .map(|(before, sent)| {
            let stamps = &all_stamps[*before..];
            assert!(stamps.len() >= PRICES, "every price was stamped");
            // Each price takes the first stamp at or after its send; a stamp another request took
            // lands between sends and is skipped, never counted against a price.
            let mut next = 0;
            let mut waits: Vec<u128> = sent
                .iter()
                .map(|sent| {
                    while stamps[next] < *sent {
                        next += 1;
                    }
                    let wait = stamps[next].duration_since(*sent).as_micros();
                    next += 1;
                    wait
                })
                .collect();
            report("price_channel_to_stamp_under_reads_and_checkpoints", waits.clone());
            waits.sort_unstable();
            (waits[waits.len() / 2], waits[waits.len() * 9 / 10])
        })
        .collect();
    let medians: Vec<u128> = stats.iter().map(|(median, _)| *median).collect();
    let p90s: Vec<u128> = stats.iter().map(|(_, p90)| *p90).collect();
    assert!(medians.iter().all(|median| *median <= MEDIAN_US), "a batch's median exceeded {MEDIAN_US}us: {medians:?}");
    assert!(p90s.iter().any(|p90| *p90 <= P90_US), "every batch's p90 exceeded {P90_US}us: {p90s:?}");
}
