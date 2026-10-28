//! V19 financial fixtures; protocol migration remains tested against actual LATEST.
//! Publisher synchronization after time admission halts, with controlled clocks and public replay.

use super::*;

use engine_core::access::AccessPolicy;
use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate};
use engine_core::command::{OrderKind, OrderRequest};
use engine_core::fixed::Rounding;
use engine_core::price::SourceQuote;
use engine_core::types::Side;
use k256::ecdsa::SigningKey;

use crate::admin::sign;
use crate::clock::Clock;
use crate::trusted_time::Sample;

pub(super) const FIRST: Millis = 1_792_530_000_000;
pub(super) const DAY: Millis = 86_400_000;
pub(super) const START: Millis = FIRST - 120_000;

pub(super) struct Fixture {
    pub(super) dir: PathBuf,
    pub(super) seq: Sequencer,
    pub(super) clock: Arc<TestClock>,
}

#[test]
fn benchmark_recovery_expiry_and_drift_wait_for_fresh_history_before_each_missed_date() {
    for (kind, observed, missed) in [
        ("expiry", false, 1),
        ("expiry", true, 1),
        ("expiry", false, 2),
        ("expiry", true, 2),
        ("drift", false, 1),
        ("drift", true, 1),
        ("drift", false, 2),
        ("drift", true, 2),
    ] {
        let name = format!("benchmark-recovery-{kind}-{observed}-{missed}");
        let mut f = Fixture::new(&name, START, true, true);
        let before = (f.seq.last_seq(), f.seq.engine.next_rollover(), f.balance());
        f.advance_to(FIRST + (missed - 1) * DAY + i64::from(observed));
        if kind == "drift" {
            f.clock.skew(60_000);
            f.seq.trust(trusted(f.clock.as_ref())).unwrap();
        }
        let fresh = f.snapshot(&[("2026-10-20", "0.015"), ("2026-10-21", "0.02003")][..missed as usize]);
        if observed {
            assert!(f.seq.sequence(Command::Tick).unwrap().is_none(), "{name}");
            assert!(f.seq.sequence(fresh.clone()).unwrap().is_none(), "snapshot during halt: {name}");
            assert_eq!((f.seq.last_seq(), f.seq.engine.next_rollover(), f.balance()), before, "{name}");
        }
        f.clock.skew(0);
        f.seq.trust(trusted(f.clock.as_ref())).unwrap();
        assert!(f.seq.sequence(Command::Tick).unwrap().is_none(), "ticker beat publisher retry: {name}");
        assert_eq!((f.seq.last_seq(), f.seq.engine.next_rollover(), f.balance()), before, "{name}");
        let events = f.submit(fresh.clone());
        let expected = [charge("0.015"), charge("0.02003")][..missed as usize].to_vec();
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
        assert_eq!(charges(&events), expected, "{name}");
        assert_eq!(f.balance(), before.2 - expected.iter().copied().sum::<Fixed>(), "{name}");
        assert_eq!(f.seq.engine.next_rollover(), Some(FIRST + missed * DAY), "{name}");

        // A lost acknowledgement cannot charge twice, with the original request or a new nonce.
        let repeated = f.submit(fresh);
        assert!(repeated.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{repeated:?}");
        assert!(charges(&repeated).is_empty());
        let repeated = f.snapshot(&[("2026-10-20", "0.015"), ("2026-10-21", "0.02003")][..missed as usize]);
        assert!(charges(&f.submit(repeated)).is_empty());
        assert!(charges(&f.submit(Command::Tick)).is_empty());
        f.assert_public_replay(&expected);
    }
}

#[test]
fn benchmark_recovery_rejected_snapshots_keep_waiting_and_complete_failed_fetches_release_it() {
    for retained in [false, true] {
        let mut f = Fixture::new(&format!("benchmark-recovery-failed-fetch-{retained}"), START, true, retained);
        let before = (f.seq.engine.next_rollover(), f.balance());
        f.advance_to(FIRST + 1);
        f.seq.trust(trusted(f.clock.as_ref())).unwrap();
        for invalid in 0..5 {
            let mut batch = f.batch(&[("2026-10-20", "0.015")]);
            match invalid {
                0 => batch.checked.clear(),
                1 => batch.rates[0].annual_rate = fixed("1.000000001"),
                2 => batch.as_of = "2026-10-19".parse().unwrap(),
                _ => {}
            }
            let cmd = f.signed(batch, if invalid == 3 { 0x43 } else { 0x42 }, if invalid == 4 { -1 } else { 60_000 });
            let events = f.submit(cmd);
            assert!(events.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{invalid}: {events:?}");
            assert!(charges(&events).is_empty());
            assert!(f.seq.benchmark_sync, "invalid snapshot must not release synchronization: {invalid}");
            assert_eq!((f.seq.engine.next_rollover(), f.balance()), before);
            assert!(f.seq.sequence(Command::Tick).unwrap().is_none());
        }
        let unsigned = Command::Admin(AdminCommand::SetBenchmarkRates(f.batch(&[])));
        assert!(f.seq.sequence(unsigned).unwrap().is_none(), "startup/recovery requires a signed attempt");
        let failed = f.snapshot(&[]);
        let events = f.submit(failed);
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
        assert!(!f.seq.benchmark_sync);
        let expected = if retained { vec![charge("0.01")] } else { vec![] };
        assert_eq!(charges(&events), expected);
        assert_eq!(f.balance(), before.1 - expected.iter().copied().sum::<Fixed>());
        assert_eq!(f.seq.engine.next_rollover(), Some(FIRST + DAY));
        if !retained {
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, Event::BenchmarkRateMissing { benchmark, .. } if benchmark == "SOFR"))
            );
        }
        assert!(charges(&f.submit(Command::Tick)).is_empty());
        f.assert_public_replay(&expected);
    }
}

#[test]
fn benchmark_recovery_brief_halt_clears_before_an_uninterrupted_rollover() {
    for observed in [false, true] {
        let mut f = Fixture::new(&format!("benchmark-recovery-brief-{observed}"), START, true, true);
        f.advance_to(FIRST - 59_000);
        if observed {
            assert!(f.seq.sequence(Command::Tick).unwrap().is_none(), "old sample has expired");
        }
        f.seq.trust(trusted(f.clock.as_ref())).unwrap();
        assert!(!f.seq.benchmark_sync, "no unpaid rollover at recovery");
        // No intervening command clears the refusal latch before this later rollover.
        f.advance_to(FIRST + 1);
        let events = f.submit(Command::Tick);
        assert_eq!(charges(&events), [charge("0.01")]);
        assert!(!f.seq.benchmark_sync);
        f.assert_public_replay(&[charge("0.01")]);
    }
}

#[test]
fn benchmark_recovery_drift_reopens_without_a_new_sample_and_only_blocks_unpaid_rollovers() {
    for overdue in [false, true] {
        let mut f = Fixture::new(&format!("benchmark-recovery-drift-self-{overdue}"), FIRST - 30_000, true, true);
        let before = f.balance();
        f.clock.skew(60_000);
        assert!(f.seq.sequence(Command::Tick).unwrap().is_none());
        f.advance_to(if overdue { FIRST + 1 } else { FIRST - 20_000 });
        f.clock.skew(0);
        let resumed = f.seq.sequence(Command::Tick).unwrap();
        let expected = if overdue {
            assert!(resumed.is_none(), "same sample admits time, but publishers must synchronize");
            assert_eq!(f.balance(), before);
            let fresh = f.snapshot(&[("2026-10-20", "0.015")]);
            assert_eq!(charges(&f.submit(fresh)), [charge("0.015")]);
            charge("0.015")
        } else {
            assert!(charges(&resumed.expect("brief drift recovery resumes sequencing")).is_empty());
            assert!(!f.seq.benchmark_sync);
            f.advance_to(FIRST + 1);
            assert_eq!(charges(&f.submit(Command::Tick)), [charge("0.01")]);
            charge("0.01")
        };
        f.assert_public_replay(&[expected]);
    }
}

#[test]
fn benchmark_recovery_required_commands_cannot_charge_before_snapshot_and_then_drain() {
    let mut f = Fixture::new("benchmark-recovery-required", START, true, true);
    let before = (f.seq.last_seq(), f.seq.engine.next_rollover(), f.balance());
    f.advance_to(FIRST + DAY + 1);
    f.seq.sequence_required("recovery test", vec![Command::Tick]).unwrap();
    f.seq.trust(trusted(f.clock.as_ref())).unwrap();
    assert_eq!((f.seq.last_seq(), f.seq.engine.next_rollover(), f.balance()), before);
    assert_eq!(f.seq.required.len(), 1, "blocked required commands remain queued");
    let snapshot = f.snapshot(&[("2026-10-20", "0.015"), ("2026-10-21", "0.02003")]);
    let events = f.submit(snapshot.clone());
    assert_eq!(charges(&events), [charge("0.015"), charge("0.02003")]);
    assert!(f.seq.required.is_empty(), "accepted snapshot releases queued commands");
    f.seq.flush().unwrap();
    let logged = commands(&log_entries(&f.dir));
    let last = logged.iter().rposition(|cmd| cmd == &snapshot).unwrap();
    assert_eq!(logged[last + 1..], [Command::Tick]);
    f.assert_public_replay(&[charge("0.015"), charge("0.02003")]);
}

#[test]
fn benchmark_recovery_disabled_publishers_keep_existing_time_recovery_behavior() {
    let mut f = Fixture::new("benchmark-recovery-disabled", START, false, true);
    f.advance_to(FIRST + DAY + 1);
    assert!(f.seq.sequence(Command::Tick).unwrap().is_none(), "expired trusted time still halts sequencing");
    f.seq.trust(trusted(f.clock.as_ref())).unwrap();
    assert_eq!(charges(&f.submit(Command::Tick)), [charge("0.01"); 2]);
    assert!(!f.seq.benchmark_sync);
    f.assert_public_replay(&[charge("0.01"); 2]);
}

#[test]
fn benchmark_recovery_v17_activation_can_precede_the_snapshot_without_charging() {
    for with_policy in [false, true] {
        let dir = scratch_dir(&format!("benchmark-recovery-v17-{with_policy}"));
        let clock = Arc::new(TestClock::at(START));
        let mut seq = recover_clock(&dir, clock.clone());
        seq.trust(trusted(clock.as_ref())).unwrap();
        seq.sequence_required(
            "v17 activation",
            vec![
                Command::Admin(AdminCommand::SetClientAuth(seq.client_auth.clone())),
                Command::Admin(AdminCommand::SetOperator(seq.operator)),
                Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V17 }),
            ],
        )
        .unwrap();
        let mut genesis = dev_config().genesis;
        genesis.markets.retain(|market| market.id == 1);
        seq.genesis(&genesis).unwrap();
        let reference = format!("{}:0", engine_core::eip712::B256::repeat_byte(17));
        assert!(seq.sequence(deposit(100, &reference)).unwrap().unwrap().iter().any(|event| {
            matches!(event, Event::Deposited { account: TRADER, amount } if *amount == Fixed::from_int(100))
        }));
        seq.flush().unwrap();
        drop(seq);
        clock.advance(Duration::from_millis((FIRST + DAY + 1 - START) as u64));
        let mut seq = recover_clock(&dir, clock.clone());
        let policy = AccessPolicy { source: AccountId([0x46; 20]), max_age_ms: 1000 };
        if with_policy {
            seq.access_policy = Some(policy.clone());
        }
        let before = seq.engine.next_rollover();
        seq.wait_for_benchmarks().unwrap();
        seq.activate_protocol().unwrap();
        seq.trust(trusted(clock.as_ref())).unwrap();
        assert_eq!(seq.engine.protocol_version(), ProtocolVersion::LATEST);
        assert!(seq.engine.deposit_finality_enabled());
        assert_eq!(seq.engine.accounts()[&TRADER].balance, Fixed::from_int(100));
        assert_eq!(seq.engine.accounts()[&TRADER].unfinalised_deposits, Fixed::from_int(100));
        assert_eq!(seq.engine.next_rollover(), before, "activation must not perform catch-up");
        assert_eq!(seq.engine.access_policy(), None, "source setup must not charge unpaid legacy rollovers");
        assert_eq!(
            seq.required.is_empty(),
            !with_policy,
            "only source setup may wait behind publisher synchronization"
        );
        assert!(seq.sequence(Command::Tick).unwrap().is_none());
        let mut f = Fixture { dir, seq, clock };
        let snapshot = f.snapshot(&[("2026-10-20", "0.015"), ("2026-10-21", "0.02003")]);
        assert!(f.submit(snapshot).contains(&Event::AdminApplied));
        assert_eq!(f.seq.engine.next_rollover(), Some(FIRST + 2 * DAY));
        assert!(f.seq.required.is_empty(), "the snapshot must finish queued source setup");
        assert_eq!(f.seq.engine.access_policy(), with_policy.then_some(&policy));
        f.assert_public_replay(&[]);
    }
}

#[tokio::test]
async fn benchmark_recovery_dropped_snapshot_acknowledgement_is_durable_and_charges_once() {
    let mut f = Fixture::new("benchmark-recovery-dropped-ack", START, true, true);
    let before = f.balance();
    let operator = f.seq.operator;
    let initial_nonce = f.seq.engine.next_request_nonce(operator.address);
    f.advance_to(FIRST + DAY + 1);
    f.seq.trust(trusted(f.clock.as_ref())).unwrap();
    let Command::SignedAdmin(request) = f.snapshot(&[("2026-10-20", "0.015"), ("2026-10-21", "0.02003")]) else {
        unreachable!("snapshot helper always signs the batch");
    };
    let expires_at = f.clock.true_ms() + 60_000;
    let dir = f.dir;
    let (tx, rx) = mpsc::channel(16);
    let task = tokio::spawn(f.seq.run(rx));
    let (reply, abandoned) = oneshot::channel();
    tx.send(Request::Submit { cmd: Command::SignedAdmin(request.clone()), reply: Some(reply) }).await.unwrap();

    let (observed, barrier) = oneshot::channel();
    tx.send(Request::Query(Box::new(move |engine| {
        let _ = observed.send((
            engine.accounts()[&TRADER].balance,
            engine.next_rollover(),
            engine.next_request_nonce(operator.address),
        ));
    })))
    .await
    .unwrap();
    let observed = tokio::time::timeout(Duration::from_secs(3), barrier)
        .await
        .expect("sequencer processes the abandoned snapshot and following query")
        .unwrap();
    // Discard the ready acknowledgement only after the query proves the snapshot was flushed.
    drop(abandoned);
    let key = SigningKey::from_bytes((&[0x42; 32]).into()).unwrap();
    let refreshed = sign(&key, &operator, request.command.clone(), observed.2, expires_at);
    let mut replies = Vec::new();
    for cmd in [Command::SignedAdmin(request), Command::SignedAdmin(Box::new(refreshed)), Command::Tick] {
        let (reply, response) = oneshot::channel();
        tx.send(Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
        replies.push(
            tokio::time::timeout(Duration::from_secs(3), response)
                .await
                .expect("snapshot retry and ticker receive bounded replies")
                .unwrap(),
        );
    }
    drop(tx);
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("sequencer drains after every sender is dropped")
        .unwrap()
        .unwrap();

    let expected = [charge("0.015"), charge("0.02003")];
    let balance = before - expected.iter().copied().sum::<Fixed>();
    assert_eq!(
        observed,
        (balance, Some(FIRST + 2 * DAY), initial_nonce + 1),
        "a dropped snapshot acknowledgement cannot discard its accepted financing or nonce"
    );
    assert!(replies[0].iter().any(|event| matches!(event, Event::AdminRejected { .. })), "original nonce is consumed");
    assert!(replies[1].contains(&Event::AdminApplied), "fresh nonce retry succeeds");
    assert!(replies.iter().all(|events| charges(events).is_empty()), "neither retry nor ticker charges twice");
    let entries = log_entries(&dir);
    let financing: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(command) => command.events.as_deref(),
            _ => None,
        })
        .flat_map(charges)
        .collect();
    assert_eq!(financing, expected, "the public log records each dated charge once");
    let replayed = replay(&entries).unwrap();
    assert_eq!(replayed.engine.accounts()[&TRADER].balance, balance);
    assert_eq!(replayed.engine.next_rollover(), Some(FIRST + 2 * DAY));
    assert_eq!(replayed.engine.next_request_nonce(operator.address), initial_nonce + 2);
}

impl Fixture {
    pub(super) fn new(name: &str, at: Millis, publishers: bool, retained: bool) -> Self {
        let dir = scratch_dir(name);
        let clock = Arc::new(TestClock::at(at));
        let mut seq = recover_clock(&dir, clock.clone());
        if publishers {
            seq.wait_for_benchmarks().unwrap();
        }
        seq.activate_legacy_protocol().unwrap();
        let mut genesis = dev_config().genesis;
        genesis.markets.retain(|market| market.id == 1);
        genesis.benchmarks.clear();
        seq.genesis(&genesis).unwrap();
        assert!(commands(&log_entries(&dir)).is_empty(), "bootstrap waits for authenticated time");
        seq.trust(trusted(clock.as_ref())).unwrap();
        assert_eq!(seq.engine.protocol_version(), ProtocolVersion::V23);
        assert!(!seq.needs_genesis(), "publisher gate must allow cold genesis");
        let mut f = Self { dir, seq, clock };
        let initial = if retained { vec![("2026-10-19", "0.01")] } else { vec![] };
        let snapshot = f.snapshot(&initial);
        assert!(f.submit(snapshot).contains(&Event::AdminApplied));
        assert!(!f.seq.benchmark_sync);
        f.submit(deposit(100_000, "benchmark-recovery-deposit"));
        f.submit(Command::Prices {
            market: 1,
            quotes: ["venue-a", "venue-b"]
                .into_iter()
                .map(|source| SourceQuote { source: source.into(), mid: fixed("100000"), ts: at, contract: None })
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

    pub(super) fn advance_to(&self, at: Millis) {
        self.clock.advance(Duration::from_millis((at - self.clock.true_ms()) as u64));
    }

    pub(super) fn batch(&self, observations: &[(&str, &str)]) -> BenchmarkBatch {
        BenchmarkBatch {
            as_of: self.seq.engine.benchmark_date(self.clock.true_ms()),
            checked: self.seq.engine.benchmark_state().required,
            rates: observations
                .iter()
                .map(|(day, rate)| BenchmarkRate {
                    benchmark: "SOFR".into(),
                    effective_date: day.parse().unwrap(),
                    annual_rate: fixed(rate),
                })
                .collect(),
        }
    }

    pub(super) fn signed(&self, batch: BenchmarkBatch, seed: u8, expires_in: Millis) -> Command {
        let key = SigningKey::from_bytes((&[seed; 32]).into()).unwrap();
        let req = sign(
            &key,
            &self.seq.operator,
            AdminCommand::SetBenchmarkRates(batch),
            self.seq.engine.next_request_nonce(self.seq.operator.address),
            self.clock.true_ms() + expires_in,
        );
        Command::SignedAdmin(Box::new(req))
    }

    pub(super) fn snapshot(&self, observations: &[(&str, &str)]) -> Command {
        self.signed(self.batch(observations), 0x42, 60_000)
    }

    pub(super) fn submit(&mut self, cmd: Command) -> Vec<Event> {
        self.seq.sequence(cmd).unwrap().expect("time and publisher synchronization admit this command")
    }

    pub(super) fn balance(&self) -> Fixed {
        self.seq.engine.accounts()[&TRADER].balance
    }

    pub(super) fn assert_public_replay(&mut self, expected: &[Fixed]) {
        self.seq.flush().unwrap();
        let entries = log_entries(&self.dir);
        let amounts: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                LogEntry::Command(command) => command.events.as_deref(),
                _ => None,
            })
            .flat_map(charges)
            .collect();
        assert_eq!(amounts, expected);
        let mut replayed = replay(&entries).unwrap();
        assert_eq!(replayed.engine.state_root(), self.seq.engine.state_root());
        assert_eq!(replayed.engine.next_rollover(), self.seq.engine.next_rollover());
    }
}

fn recover_clock(dir: &Path, clock: Arc<TestClock>) -> Sequencer {
    let genesis = finished_genesis(&log_entries(dir));
    let cfg = SequencerConfig { clock, genesis, ..sequencer_config(dir, watch::channel(Manifest::default()).0) };
    Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(16).0, mpsc::unbounded_channel().0)
        .unwrap()
}

pub(super) fn trusted(clock: &TestClock) -> Sample {
    Sample { at: clock.instant(), earliest_ms: clock.true_ms() - 1_000, latest_ms: clock.true_ms() + 1_000 }
}

fn fixed(value: &str) -> Fixed {
    value.parse().unwrap()
}

fn charge(rate: &str) -> Fixed {
    fixed("10000").mul_rounded(fixed(rate) + Fixed::from_bps(500), Rounding::Up).div_int_rounded(365, Rounding::Up)
}

pub(super) fn charges(events: &[Event]) -> Vec<Fixed> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Financing { market: 1, amount, .. } => Some(*amount),
            _ => None,
        })
        .collect()
}
