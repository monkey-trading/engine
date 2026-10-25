use super::*;
use crate::log::Checkpoint;

use engine_core::command::{OrderKind, OrderRequest};
use engine_core::price::SourceQuote;
use engine_core::types::Side;

use tokio::sync::oneshot;
use tokio::time::advance;

use crate::alerts::{Severity, capture};
use crate::clock::test_clock::TestClock;
use crate::log::{Manifest, SegmentedLog};
use crate::metrics::Metrics;
use crate::test_support::{
    SEGMENT_COMMANDS, TRADER, TestSequencer, deposit, dev_config, sample_around, scratch_dir, sequencer_config,
};
use crate::throttle::REALERT_AFTER;

#[tokio::test]
async fn sequencing_counts_commands_fills_and_rejections_by_reason_and_times_checkpoints() {
    let dir = scratch_dir("seq-metrics");
    // A clock that stands still, so the quotes stay fresh however slowly the commands go through.
    let clock = Arc::new(TestClock::default());
    let node = TestSequencer::start_at(&dir, 64, clock.clone(), genesis_commands(&dev_config().genesis));
    let genesis_commands = node.metrics.commands.get();
    let ts = clock.now_ms();
    let quotes =
        ["a", "b", "c"].map(|v| SourceQuote { source: v.into(), mid: Fixed::from_int(100_000), ts, contract: None });
    node.submit(Command::Prices { market: 1, quotes: quotes.to_vec() }).await;
    node.submit(deposit(1_000, "dep-1")).await;
    let filled = node.submit(buy("0.01")).await;
    assert!(filled.iter().any(|e| matches!(e, Event::Fill(_))), "{filled:?}");
    let refused = node.submit(buy("1")).await;
    assert!(refused.iter().any(|e| matches!(e, Event::OrderRejected { .. })), "{refused:?}");

    let metrics = node.metrics.clone();
    assert_eq!(metrics.commands.get(), genesis_commands + 4);
    let text = metrics.encode();
    assert!(text.contains("engine_fills_total{market=\"1\"} 1\n"), "{text}");
    let margin = "engine_rejections_total{event=\"order_rejected\",reason=\"insufficient_margin\"} 1\n";
    assert!(text.contains(margin), "{text}");
    assert!(!text.contains(&TRADER.to_string()), "{text}");
    node.stop().await;
    let checkpointed = Sequencer::read_log(&dir, |_| {}).unwrap().replayed.last_checkpoint_ts.unwrap();
    assert_eq!(metrics.last_checkpoint_seconds.get(), checkpointed as f64 / 1000.0);
}

#[tokio::test]
async fn custody_rejections_from_sequencing_are_counted_and_keep_critical_alerts() {
    let dir = scratch_dir("seq-custody-rejection-metrics");
    let node =
        TestSequencer::start_at(&dir, 64, Arc::new(TestClock::default()), genesis_commands(&dev_config().genesis));
    let mut receipt = crate::test_support::every_event()
        .into_iter()
        .find_map(|event| match event {
            Event::CustodyDepositRejected { receipt, .. } => Some(receipt),
            _ => None,
        })
        .expect("custody rejection fixture");
    receipt.amount = Fixed::ZERO;
    let events =
        node.submit(Command::CustodyDeposit(Box::new(engine_core::deposits::DepositUpdate::Credit { receipt }))).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::CustodyDepositRejected { reason: engine_core::deposits::DepositReject::InvalidAmount, .. }
        )),
        "{events:?}"
    );
    let text = node.metrics.encode();
    assert!(
        text.contains("engine_rejections_total{event=\"custody_deposit_rejected\",reason=\"invalid_amount\"} 1\n"),
        "{text}"
    );
    let lines = capture(|| events.iter().for_each(alert_on));
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].severity, Some(Severity::Critical));
    node.stop().await;
}

#[test]
fn recovery_sets_the_checkpoint_time_from_the_command_it_covers() {
    let dir = scratch_dir("seq-checkpoint-time");
    let mut engine = Engine::new();
    let first = Sequenced { seq: 0, ts: 1_000, cmd: Command::Tick };
    let events = engine.apply(&first).unwrap();
    let root = format!("0x{}", hex::encode(engine.state_root()));
    let second = Sequenced { seq: 1, ts: 5_000, cmd: Command::Tick };
    let mut log = SegmentedLog::open(&dir, |_| Ok(()))
        .unwrap()
        .into_writer(SEGMENT_COMMANDS, watch::channel(Manifest::default()).0)
        .unwrap();
    log.append(&LogEntry::Command(CommandEntry { seal: None, sequenced: first, events: Some(events) })).unwrap();
    let checkpoint = checkpoint(0, root);
    log.append(&checkpoint).unwrap();
    let LogEntry::Checkpoint(cp) = &checkpoint else { unreachable!() };
    let mut second = CommandEntry { sequenced: second, events: Some(Vec::new()), seal: None };
    let previous = crate::log::command_seal::checkpoint_link(cp).unwrap();
    let link = crate::log::command_seal::command_link(previous, &second).unwrap();
    second.seal = Some(EnclaveKey::test_key().seal_command(cp.domain.unwrap(), link));
    log.append(&LogEntry::Command(second)).unwrap();
    log.flush().unwrap();
    drop(log);

    let metrics = Arc::new(Metrics::new());
    let (stream, _) = broadcast::channel(16);
    let key = Arc::new(EnclaveKey::test_key());
    let manifest = watch::channel(Manifest::default()).0;
    let cfg = SequencerConfig { metrics: metrics.clone(), ..sequencer_config(&dir, manifest) };
    Sequencer::recover(cfg, key, stream, mpsc::unbounded_channel().0).unwrap();
    assert_eq!(metrics.last_checkpoint_seconds.get(), 1.0);
}

/// A batch is timed once, on the sequencer's clock, across every command it sequences: each stamp
/// here takes 5ms of that clock, so a batch of two records exactly 10ms.
#[tokio::test]
async fn a_batch_is_timed_once_across_every_command_it_sequences() {
    let metrics = Arc::new(Metrics::new());
    let (stream, _) = broadcast::channel(16);
    let dir = scratch_dir("seq-batch");
    let manifest = watch::channel(Manifest::default()).0;
    let key = Arc::new(EnclaveKey::test_key());
    let clock = Arc::new(SlowStamps(TestClock::default()));
    let cfg = SequencerConfig { clock: clock.clone(), metrics: metrics.clone(), ..sequencer_config(&dir, manifest) };
    let mut seq = Sequencer::recover(cfg, key, stream, mpsc::unbounded_channel().0).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let (tx, rx) = mpsc::channel(8);
    let mut replies = Vec::new();
    for _ in 0..2 {
        let (reply, events) = oneshot::channel();
        tx.send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).await.unwrap();
        replies.push(events);
    }
    drop(tx);
    seq.run(rx).await.unwrap();
    for events in replies {
        events.await.unwrap();
    }
    let text = metrics.encode();
    assert!(text.contains("engine_sequencer_batch_seconds_count 1\n"), "{text}");
    assert!(text.contains("engine_sequencer_batch_seconds_sum 0.01\n"), "{text}");
    assert!(text.contains("engine_commands_total 2\n"), "{text}");
}

#[tokio::test]
async fn a_batch_of_reads_alone_is_not_timed() {
    let metrics = Arc::new(Metrics::new());
    let (stream, _) = broadcast::channel(16);
    let dir = scratch_dir("seq-batch-reads");
    let manifest = watch::channel(Manifest::default()).0;
    let key = Arc::new(EnclaveKey::test_key());
    let cfg = SequencerConfig { metrics: metrics.clone(), ..sequencer_config(&dir, manifest) };
    let seq = Sequencer::recover(cfg, key, stream, mpsc::unbounded_channel().0).unwrap();
    let (tx, rx) = mpsc::channel(8);
    let (reply, answered) = oneshot::channel();
    tx.send(Request::Query(Box::new(move |_: &Engine| {
        let _ = reply.send(());
    })))
    .await
    .unwrap();
    drop(tx);
    seq.run(rx).await.unwrap();
    answered.await.unwrap();
    assert!(metrics.encode().contains("engine_sequencer_batch_seconds_count 0\n"));
}

#[test]
fn a_price_rejection_alert_keys_on_its_market_alone() {
    let reason = "quote from venue-a stamped 5000ms ahead of the sequenced time".to_owned();
    let lines = capture(|| alert_on(&Event::PricesRejected { market: 1, reason }));
    let [line] = lines.as_slice() else { panic!("{lines:?}") };
    assert_eq!(line.key(), "prices_rejected{market=1}");
    assert!(line.summary.ends_with("5000ms ahead of the sequenced time"), "{}", line.summary);
}

/// A house deficit pages once, resolves only after NAV has stayed at or above zero for
/// `REALERT_AFTER`, so a house crossing zero with every price does not churn pages, and pages again
/// when a deficit returns after that.
#[tokio::test(start_paused = true)]
async fn a_house_deficit_resolves_once_nav_stays_at_or_above_zero() {
    let mut deficit = DeficitAlert::default();
    // The alert lines one reading of house NAV logs, as (key, resolve).
    let mut note = |units: i64| -> Vec<(String, bool)> {
        let lines = capture(|| deficit.note(Fixed::from_int(units)));
        lines.iter().map(|line| (line.key(), line.severity.is_none())).collect()
    };
    let (fired, resolved) = (("house_deficit{}".to_owned(), false), ("house_deficit{}".to_owned(), true));
    let almost = REALERT_AFTER - Duration::from_millis(1);
    assert_eq!(note(-1), std::slice::from_ref(&fired));
    assert!(note(1).is_empty());
    advance(almost).await;
    assert!(note(1).is_empty());
    // A dip restarts the clock without a new page.
    assert!(note(-1).is_empty());
    assert!(note(1).is_empty());
    advance(almost).await;
    assert!(note(1).is_empty());
    advance(Duration::from_millis(1)).await;
    assert_eq!(note(1), [resolved]);
    assert_eq!(note(-1), [fired]);
}

// ── Helpers ──

fn checkpoint(seq: u64, root: String) -> LogEntry {
    let key = EnclaveKey::test_key();
    let domain = crate::log::CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
    let signature = key.sign_checkpoint(
        domain.chain_id,
        &domain.vault.0,
        seq,
        &root.parse::<engine_core::eip712::B256>().unwrap_or_default().0,
    );
    LogEntry::Checkpoint(Checkpoint {
        parent: None,
        seq,
        root,
        signature,
        signer: key.address_hex(),
        domain: Some(domain),
        deferred_withdrawals: false,
    })
}

/// A market buy of `qty` BTC by `TRADER` at the test mid.
fn buy(qty: &str) -> Command {
    Command::PlaceOrder(OrderRequest {
        account: TRADER,
        client_id: 1,
        market: 1,
        side: Side::Buy,
        qty: qty.parse().unwrap(),
        kind: OrderKind::Market { quoted: Fixed::from_int(100_000), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    })
}

/// A host clock on which each wall-clock read, one per stamp, takes 5ms of monotonic time.
struct SlowStamps(TestClock);

impl Clock for SlowStamps {
    fn now_ms(&self) -> Millis {
        self.0.advance(Duration::from_millis(5));
        self.0.now_ms()
    }

    fn instant(&self) -> std::time::Instant {
        self.0.instant()
    }
}
