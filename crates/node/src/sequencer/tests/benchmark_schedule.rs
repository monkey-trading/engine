//! Accepted rollover changes invalidate publisher work without stopping pre-deadline commands.

use super::*;

use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate};
use engine_core::command::{OrderKind, OrderRequest};
use engine_core::fixed::Rounding;
use engine_core::price::SourceQuote;
use engine_core::types::Side;
use k256::ecdsa::SigningKey;

use crate::admin::sign;
use crate::clock::Clock;

const FIRST: Millis = 1_792_530_000_000;
const START: Millis = FIRST - 120_000;

struct Fixture {
    dir: PathBuf,
    seq: Sequencer,
    clock: Arc<TestClock>,
}

#[test]
fn changed_rollover_waits_only_when_due_and_rejects_old_work_before_charging_once() {
    let mut f = Fixture::new("benchmark-schedule-due", true);
    let old_work = f.snapshot("0.01");
    let mut params = f.seq.engine.params().unwrap().clone();
    params.rollover.hour = 21;
    params.rollover.minute = 59;
    let changed = f.signed(AdminCommand::SetParams(params));
    assert!(f.submit(changed).contains(&Event::AdminApplied));
    assert!(f.seq.benchmark_schedule_pending);
    assert!(!f.seq.benchmark_sync, "accepted schedule change permits commands before its deadline");
    assert!(charges(&f.submit(Command::Tick)).is_empty());
    let due = FIRST - 60_000;
    assert_eq!(f.seq.engine.next_rollover(), Some(due));
    let before = (f.seq.last_seq(), f.balance(), f.seq.engine.house_cash());
    f.advance(due + 1);
    assert!(!f.seq.benchmark_time_refused, "trusted time remains continuously healthy in this schedule test");
    assert!(f.seq.benchmark_sync, "time samples promote the due schedule barrier before queued commands");
    assert!(f.seq.sequence(Command::Tick).unwrap().is_none());
    assert_eq!((f.seq.last_seq(), f.balance(), f.seq.engine.house_cash()), before);
    let rejected = f.submit(old_work);
    assert!(rejected.iter().any(|event| matches!(event, Event::AdminRejected { reason } if reason.contains("nonce"))));
    assert!(charges(&rejected).is_empty());
    assert!(f.seq.benchmark_schedule_pending && f.seq.benchmark_sync);
    let fresh = f.snapshot("0.015");
    let applied = f.submit(fresh.clone());
    let expected = charge("0.015");
    assert!(applied.contains(&Event::AdminApplied));
    assert_eq!(charges(&applied), [expected]);
    assert!(applied.iter().any(|event| matches!(event, Event::BenchmarkRateObserved {
        benchmark, effective_date, rollover_date, stale: false,
    } if benchmark == "SOFR" && *effective_date == "2026-10-20".parse().unwrap()
        && effective_date == rollover_date)));
    assert_eq!(f.balance(), before.1 - expected);
    assert_eq!(f.seq.engine.house_cash(), before.2 + expected);
    assert!(!f.seq.benchmark_schedule_pending && !f.seq.benchmark_sync);
    assert!(charges(&f.submit(fresh)).is_empty(), "lost acknowledgement cannot repeat financing");
    assert!(charges(&f.submit(Command::Tick)).is_empty());
    f.assert_replay(&[expected]);
}

#[test]
fn changed_rollover_accepts_a_fresh_predeadline_attempt_and_preserves_failed_fetch_policy() {
    for failed in [false, true] {
        let mut f = Fixture::new(&format!("benchmark-schedule-before-due-{failed}"), true);
        let mut params = f.seq.engine.params().unwrap().clone();
        params.rollover.minute = 1;
        let changed = f.signed(AdminCommand::SetParams(params));
        f.submit(changed);
        assert!(f.seq.benchmark_schedule_pending);
        let mut snapshot = f.batch("0.015");
        if failed {
            snapshot.rates.clear();
        }
        let fresh = f.signed(AdminCommand::SetBenchmarkRates(snapshot));
        assert!(charges(&f.submit(fresh)).is_empty());
        assert!(!f.seq.benchmark_schedule_pending);
        f.advance(FIRST + 60_001);
        let expected = charge(if failed { "0.01" } else { "0.015" });
        assert_eq!(charges(&f.submit(Command::Tick)), [expected]);
        f.assert_replay(&[expected]);
    }
}

#[test]
fn only_accepted_actual_schedule_changes_arm_enabled_publishers() {
    for enabled in [false, true] {
        let mut f = Fixture::new(&format!("benchmark-schedule-filter-{enabled}"), enabled);
        let initial = f.seq.benchmark_schedule.borrow().clone();
        let mut params = f.seq.engine.params().unwrap().clone();
        params.rollover.hour = 24;
        let rejected = f.signed(AdminCommand::SetParams(params));
        assert!(f.submit(rejected).iter().any(|event| matches!(event, Event::AdminRejected { .. })));
        let mut unchanged = f.seq.engine.params().unwrap().clone();
        unchanged.profit_fee_bps += 1;
        let unchanged = f.signed(AdminCommand::SetParams(unchanged));
        assert!(f.submit(unchanged).contains(&Event::AdminApplied));
        assert!(!f.seq.benchmark_schedule_pending);
        assert_eq!(*f.seq.benchmark_schedule.borrow(), initial);
        let mut changed = f.seq.engine.params().unwrap().clone();
        changed.rollover.minute = 1;
        let changed = f.signed(AdminCommand::SetParams(changed));
        assert!(f.submit(changed).contains(&Event::AdminApplied));
        assert_eq!(f.seq.benchmark_schedule_pending, enabled);
        assert!(f.seq.benchmark_schedule.borrow().revision > initial.revision);
        if !enabled {
            f.advance(FIRST + 60_001);
            assert_eq!(charges(&f.submit(Command::Tick)), [charge("0.01")]);
            assert!(!f.seq.benchmark_sync);
        }
    }
}

#[test]
fn schedule_watch_publishes_after_flush_and_keeps_a_to_b_to_a_revision_across_restart() {
    let mut f = Fixture::new("benchmark-schedule-durable", true);
    let mut updates = f.seq.benchmark_schedule.subscribe();
    let initial = updates.borrow_and_update().clone();
    let original = f.seq.engine.params().unwrap().clone();
    let mut changed = original.clone();
    changed.rollover.minute = 1;
    for params in [changed, original] {
        let command = f.signed(AdminCommand::SetParams(params));
        assert!(f.seq.sequence(command).unwrap().unwrap().contains(&Event::AdminApplied));
        assert_eq!(*updates.borrow(), initial, "unflushed schedule cannot escape through the watch");
        assert!(!updates.has_changed().unwrap());
    }
    f.seq.flush().unwrap();
    let durable = updates.borrow_and_update().clone();
    assert!(durable.revision > initial.revision);
    assert_eq!(durable.rollover, initial.rollover, "an equal final schedule still invalidates old work");
    assert!(f.seq.benchmark_schedule_pending);
    let expected_root = f.seq.engine.state_root();
    drop(f.seq);
    let mut recovered = recover_at(&f.dir, f.clock.clone());
    assert_eq!(*recovered.benchmark_schedule.borrow(), durable);
    assert_eq!(recovered.engine.state_root(), expected_root);
    recovered.wait_for_benchmarks().unwrap();
    assert!(recovered.benchmark_sync, "restart always establishes its existing startup synchronization");
}

#[tokio::test]
async fn schedule_subscriptions_reply_after_the_containing_batch_is_durable() {
    let f = Fixture::new("benchmark-schedule-subscribe", true);
    let mut params = f.seq.engine.params().unwrap().clone();
    params.rollover.minute = 1;
    let command = f.signed(AdminCommand::SetParams(params));
    let (tx, rx) = mpsc::channel(4);
    tx.send(Request::Submit { cmd: command, reply: None }).await.unwrap();
    let (reply, subscribed) = oneshot::channel();
    tx.send(Request::WatchBenchmarkSchedule(reply)).await.unwrap();
    let task = tokio::spawn(f.seq.run(rx));
    let updates = tokio::time::timeout(Duration::from_secs(5), subscribed).await.unwrap().unwrap();
    let view = updates.borrow().clone();
    let entries = log_entries(&f.dir);
    assert!(entries.iter().any(|entry| matches!(entry, LogEntry::Command(command)
        if command.sequenced.seq + 1 == view.revision
        && command.events.as_ref().unwrap().contains(&Event::AdminApplied))));
    assert_eq!(view.rollover.as_ref().unwrap().minute, 1);
    drop(updates);
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap().unwrap();
}

impl Fixture {
    fn new(name: &str, publishers: bool) -> Self {
        let dir = scratch_dir(name);
        let clock = Arc::new(TestClock::at(START));
        let mut seq = recover_at(&dir, clock.clone());
        if publishers {
            seq.wait_for_benchmarks().unwrap();
        }
        seq.activate_protocol().unwrap();
        let mut genesis = dev_config().genesis;
        genesis.markets.retain(|market| market.id == 1);
        genesis.benchmarks.clear();
        seq.genesis(&genesis).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        let mut f = Self { dir, seq, clock };
        let snapshot = f.snapshot("0.01");
        f.submit(snapshot);
        f.submit(deposit(100_000, "benchmark-schedule-deposit"));
        f.submit(Command::Prices {
            market: 1,
            quotes: ["venue-a", "venue-b"]
                .into_iter()
                .map(|source| SourceQuote { source: source.into(), mid: fixed("100000"), ts: START })
                .collect(),
        });
        let events = f.submit(Command::PlaceOrder(OrderRequest {
            account: TRADER,
            client_id: 1,
            market: 1,
            side: Side::Buy,
            qty: fixed("0.1"),
            kind: OrderKind::Market { quoted: fixed("100000"), max_slippage_bps: None },
            reduce_only: false,
            auth: None,
        }));
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
        f
    }

    fn advance(&mut self, at: Millis) {
        while self.clock.now_ms() < at {
            let next = (self.clock.now_ms() + 30_000).min(at);
            self.clock.advance(Duration::from_millis((next - self.clock.now_ms()) as u64));
            self.seq.trust(sample_around(self.clock.as_ref())).unwrap();
        }
    }

    fn signed(&self, command: AdminCommand) -> Command {
        let key = SigningKey::from_slice(&[0x42; 32]).unwrap();
        Command::SignedAdmin(Box::new(sign(
            &key,
            &self.seq.operator,
            command,
            self.seq.engine.next_request_nonce(self.seq.operator.address),
            self.clock.now_ms() + 150_000,
        )))
    }

    fn batch(&self, rate: &str) -> BenchmarkBatch {
        BenchmarkBatch {
            as_of: self.seq.engine.benchmark_date(self.clock.now_ms()),
            checked: self.seq.engine.benchmark_state().required,
            rates: vec![BenchmarkRate {
                benchmark: "SOFR".into(),
                effective_date: "2026-10-20".parse().unwrap(),
                annual_rate: fixed(rate),
            }],
        }
    }

    fn snapshot(&self, rate: &str) -> Command {
        self.signed(AdminCommand::SetBenchmarkRates(self.batch(rate)))
    }

    fn submit(&mut self, command: Command) -> Vec<Event> {
        let events = self.seq.sequence(command).unwrap().expect("command admitted");
        self.seq.flush().unwrap();
        events
    }

    fn balance(&self) -> Fixed {
        self.seq.engine.accounts()[&TRADER].balance
    }

    fn assert_replay(&mut self, expected: &[Fixed]) {
        self.seq.flush().unwrap();
        let entries = log_entries(&self.dir);
        let financing: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                LogEntry::Command(command) => command.events.as_deref(),
                _ => None,
            })
            .flat_map(charges)
            .collect();
        assert_eq!(financing, expected);
        let mut replayed = replay(&entries).unwrap();
        assert_eq!(replayed.engine.state_root(), self.seq.engine.state_root());
        assert_eq!(replayed.engine.accounts()[&TRADER].balance, self.balance());
    }
}

fn recover_at(dir: &Path, clock: Arc<TestClock>) -> Sequencer {
    let genesis = finished_genesis(&log_entries(dir));
    let cfg = SequencerConfig { clock, genesis, ..sequencer_config(dir, watch::channel(Manifest::default()).0) };
    Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(16).0, mpsc::unbounded_channel().0)
        .unwrap()
}

fn fixed(value: &str) -> Fixed {
    value.parse().unwrap()
}

fn charge(rate: &str) -> Fixed {
    fixed("10000").mul_rounded(fixed(rate) + Fixed::from_bps(500), Rounding::Up).div_int_rounded(365, Rounding::Up)
}

fn charges(events: &[Event]) -> Vec<Fixed> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Financing { market: 1, amount, .. } => Some(*amount),
            _ => None,
        })
        .collect()
}
