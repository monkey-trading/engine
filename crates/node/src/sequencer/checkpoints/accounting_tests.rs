//! Exact V19 financial vectors and current-version capture over migrated credits plus fresh holds.
use std::collections::BTreeMap;
use std::hint::black_box;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine_core::Fixed;
use engine_core::command::{Command, OrderKind, OrderRequest};
use engine_core::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositStatus, DepositUpdate};
use engine_core::eip712::B256;
use engine_core::event::{Event, Reject};
use engine_core::price::SourceQuote;
use engine_core::types::{AccountId, Millis, Side};
use tokio::sync::{broadcast, mpsc, watch};

use crate::checkpoints::Work;
use crate::checkpoints::accounting::tests::{approved, assert_matches, recompute, recompute_work};
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::log::{CheckpointParent, Manifest};
use crate::sequencer::replay::accounting_tests::{assert_replay, recovery_capture_p50};
use crate::sequencer::{Sequencer, SequencerConfig};
use crate::tee::EnclaveKey;
use crate::test_support::{TRADER, deposit, dev_config, log_entries, sample_around, scratch_dir, sequencer_config};

/// Tuesday 20 October 2026, during the launch markets' trading hours.
const START: Millis = 1_792_504_800_000;

#[test]
fn gated_live_and_recovered_accounting_matches_cash_through_trading_and_withdrawals() {
    let genesis = dev_config().genesis;
    let rollover = genesis.params.rollover.next_after(START);
    let clock = Arc::new(TestClock::at(rollover - 600_000));
    let dir = scratch_dir("accounting-live-recovery");
    let key = Arc::new(EnclaveKey::test_key());
    let (sink, mut queued) = mpsc::channel(128);
    let registry = Arc::new(Mutex::new(BTreeMap::new()));
    let cfg = || SequencerConfig {
        clock: clock.clone(),
        checkpoint_every: 1,
        checkpoints: Some(sink.clone()),
        checkpoint_registry: Some(registry.clone()),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq =
        Sequencer::recover(cfg(), key.clone(), broadcast::channel(128).0, mpsc::unbounded_channel().0).unwrap();
    seq.activate_legacy_protocol().unwrap();
    seq.genesis(&genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let mut bootstrap = Vec::new();
    while let Ok(work) = queued.try_recv() {
        bootstrap.push(work);
    }
    let finalized = bootstrap.last().unwrap().checkpoint.clone();
    seq.checkpoint_status(Some(finalized.seq), true).unwrap();
    let mut unpaid = Fixed::ZERO;
    let mut live = Vec::new();
    for cmd in [
        deposit(1000, "0xmargin"),
        Command::HouseCapital { amount: Fixed::from_int(100), chain_ref: "0xcapital".into() },
        Command::HouseCapital { amount: Fixed::from_int(-10), chain_ref: "0xcapital-out".into() },
        Command::HouseCapital { amount: Fixed::ZERO, chain_ref: "0xcapital-zero".into() },
        deposit(1000, "0xmargin"),
        Command::HouseCapital { amount: Fixed::ONE, chain_ref: "0xmargin".into() },
        prices(1, "100000", clock.now_ms()),
        order(1, Side::Buy, "0.1", "100000"),
        prices(1, "94740", clock.now_ms()),
    ] {
        live.extend(apply(&mut seq, &mut queued, &mut unpaid, cmd).1);
    }
    move_to(&mut seq, &clock, rollover - 500);
    live.extend(apply(&mut seq, &mut queued, &mut unpaid, prices(1, "94740", clock.now_ms())).1);
    move_to(&mut seq, &clock, rollover + 100);
    let (events, work) = apply(&mut seq, &mut queued, &mut unpaid, Command::Tick);
    live.extend(work);
    assert!(events.iter().any(|event| matches!(event, Event::Financing { amount, .. } if amount.is_positive())));
    assert!(events.iter().any(|event| matches!(event, Event::CloseOut { .. })));
    assert!(events.iter().any(|event| matches!(event, Event::Fill(fill) if fill.liquidation)));

    let (events, work) = apply(
        &mut seq,
        &mut queued,
        &mut unpaid,
        Command::Withdraw { account: TRADER, amount: Fixed::from_int(10_000), auth: None },
    );
    live.extend(work);
    assert!(events.iter().any(|event| matches!(event, Event::WithdrawalRejected { .. })));
    live.extend(apply(&mut seq, &mut queued, &mut unpaid, prices(2, "4000", clock.now_ms())).1);
    let (events, work) = apply(&mut seq, &mut queued, &mut unpaid, order(2, Side::Buy, "0.1", "4000"));
    live.extend(work);
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))));
    live.extend(apply(&mut seq, &mut queued, &mut unpaid, prices(2, "4100", clock.now_ms())).1);
    let (events, work) = apply(&mut seq, &mut queued, &mut unpaid, order(2, Side::Sell, "0.1", "4100"));
    live.extend(work);
    assert!(events.iter().any(|event| matches!(event, Event::Fill(fill) if fill.profit_fee.is_positive())));
    let (events, work) = apply(
        &mut seq,
        &mut queued,
        &mut unpaid,
        Command::Withdraw { account: TRADER, amount: Fixed::from_int(40), auth: None },
    );
    live.extend(work);
    assert!(events.iter().any(|event| matches!(event, Event::WithdrawalApproved { .. })));
    assert_eq!(unpaid, Fixed::from_int(40));
    let expected = recompute(&seq.engine, unpaid);
    assert_eq!(expected.1, 4, "one client deposit and three successful house references");
    let last = seq.last_seq().unwrap();
    seq.checkpoint_status(Some(last), true).unwrap();
    assert_matches(seq.accounting, &seq.engine, unpaid);
    seq.flush().unwrap();
    let entries = log_entries(&dir);
    let replayed = assert_replay(&entries);
    assert_eq!(replayed.unpaid, unpaid, "signature release is not a settled payout");
    drop(seq);

    let config = SequencerConfig {
        checkpoint_parent: CheckpointParent {
            has_checkpoint: true,
            seq: finalized.seq,
            root: finalized.root.parse().unwrap(),
        },
        ..cfg()
    };
    let mut recovered =
        Sequencer::recover(config, key, broadcast::channel(128).0, mpsc::unbounded_channel().0).unwrap();
    assert_matches(recovered.accounting, &recovered.engine, unpaid);
    let restored = recovered.take_recovery_checkpoints();
    assert_eq!(restored.len(), live.len());
    for (actual, expected) in restored.iter().zip(&live) {
        assert_eq!(actual.checkpoint.signature, expected.checkpoint.signature);
        assert_eq!(actual.checkpoint.seq, expected.checkpoint.seq);
        assert_eq!(
            (actual.liabilities, actual.deposits, actual.coverage),
            (expected.liabilities, expected.deposits, expected.coverage)
        );
    }
    recovered.trust(sample_around(clock.as_ref())).unwrap();
    let (events, _) = apply(&mut recovered, &mut queued, &mut unpaid, deposit(7, "0xafter-restart"));
    assert!(events.iter().any(|event| matches!(event, Event::Deposited { .. })));
    assert_eq!(recompute(&recovered.engine, unpaid), (expected.0 + Fixed::from_int(7), expected.1 + 1));
}

#[test]
#[ignore = "100k-account/deposit gated capture benchmark: run in an isolated CPU window"]
fn gated_work_capture_scaling() {
    let (small, small_replay, small_scan) = capture_population(1_000);
    let (large, large_replay, large_scan) = capture_population(100_000);
    assert!(
        large <= small.saturating_mul(10) + Duration::from_micros(100),
        "capture scaled with population: {small:?} -> {large:?}"
    );
    assert!(
        large_scan > large.saturating_mul(5),
        "full-state oracle should cost materially more: capture={large:?}, scan={large_scan:?}"
    );
    assert!(
        large_replay <= small_replay.saturating_mul(4) + Duration::from_micros(100),
        "outstanding replay capture scaled with population: {small_replay:?} -> {large_replay:?}"
    );
    println!(
        "ACCOUNTING scaling live_p50={small:?}/{large:?} replay_p50={small_replay:?}/{large_replay:?} scan_p50={small_scan:?}/{large_scan:?}"
    );
}

#[test]
fn gated_capture_fixture_uses_current_receipts_and_independently_verified_cash() {
    capture_population(4);
}

/// Builds real credited state, then measures the production gate's capture, registry, and send.
fn capture_population(count: u64) -> (Duration, Duration, Duration) {
    let clock = Arc::new(TestClock::at(START));
    let dir = scratch_dir("accounting-capture-scaling");
    let (sink, mut queued) = mpsc::channel(128);
    let registry = Arc::new(Mutex::new(BTreeMap::new()));
    let config = SequencerConfig {
        clock: clock.clone(),
        checkpoint_every: u64::MAX,
        segment_commands: u64::MAX,
        checkpoints: Some(sink),
        checkpoint_registry: Some(registry.clone()),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(
        config,
        Arc::new(EnclaveKey::test_key()),
        broadcast::channel(16).0,
        mpsc::unbounded_channel().0,
    )
    .unwrap();
    seq.activate_legacy_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    while queued.try_recv().is_ok() {}
    seq.checkpoint_status(seq.last_seq(), true).unwrap();
    seq.sequence(Command::CustodyFinalised(CustodyCheckpoint {
        block: 0,
        block_hash: B256::repeat_byte(1),
        deposit_count: 0,
    }))
    .unwrap()
    .unwrap();
    assert_eq!(seq.engine.custody_deposit_coverage(), Some(0));
    for index in 1..=count {
        let receipt = population_receipt(index);
        let cmd = Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: receipt.clone() }));
        let events = seq.sequence(cmd).unwrap().unwrap();
        assert!(events.contains(&Event::CustodyDepositChanged {
            receipt,
            status: DepositStatus::Unfinalised,
            principal_delta: Fixed::from_int(10),
            uncredited: None,
        }));
        if index.is_multiple_of(1024) {
            seq.flush().unwrap();
        }
    }
    assert_eq!(seq.engine.accounts()[&account(1)].unfinalised_deposits, Fixed::from_int(10));
    let withdraw = Command::Withdraw { account: account(1), amount: Fixed::ONE, auth: None };
    let events = seq.sequence(withdraw.clone()).unwrap().unwrap();
    assert!(events.iter().any(|event| matches!(event,
        Event::WithdrawalRejected { reason: Reject::InsufficientFunds { available }, .. }
        if *available == Fixed::ZERO)));
    let receipt = population_receipt(1);
    let observed_at_seq = seq.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    let events = seq
        .sequence(Command::CustodyDeposit(Box::new(DepositUpdate::Finalise {
            receipt: receipt.clone(),
            observed_at_seq,
        })))
        .unwrap()
        .unwrap();
    assert!(events.contains(&Event::CustodyDepositChanged {
        receipt,
        status: DepositStatus::Finalised,
        principal_delta: Fixed::ZERO,
        uncredited: None,
    }));
    assert_eq!(seq.engine.accounts()[&account(1)].balance, Fixed::from_int(10));
    assert_eq!(seq.engine.accounts()[&account(1)].unfinalised_deposits, Fixed::ZERO);
    assert_eq!(seq.engine.custody_deposit_coverage(), Some(count));
    // An approval forces the real gated checkpoint after the entire population has been credited.
    let events = seq.sequence(withdraw).unwrap().unwrap();
    assert_eq!(approved(&events), Fixed::ONE);
    while queued.try_recv().is_ok() {}
    seq.activate_protocol().unwrap();
    assert_eq!(seq.engine.protocol_version(), engine_core::protocol::ProtocolVersion::LATEST);
    while queued.try_recv().is_ok() {}
    // Fresh V21 raw ingress names no payer, so it leaves one tracked hold per migrated credited account.
    for index in 1..=count {
        let events = seq
            .sequence(Command::Deposit {
                account: account(index),
                amount: Fixed::from_int(10),
                chain_ref: format!("0x{:064x}:0", count + index),
            })
            .unwrap()
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [Event::DepositHeld { reason: engine_core::access::AccessReject::PayerMissing, .. }]
        ));
    }
    seq.checkpoint().unwrap();
    seq.flush().unwrap();
    assert_eq!(seq.engine.accounts().len(), count as usize);
    assert_eq!(seq.engine.held_deposits().len(), count as usize);
    assert_matches(seq.accounting, &seq.engine, Fixed::ONE);
    let expected = recompute(&seq.engine, Fixed::ONE);
    assert_eq!(expected.1, 2 * count);
    let forced = queued.try_recv().unwrap();
    assert!(queued.try_recv().is_err());
    assert_eq!((forced.liabilities, forced.deposits), expected);
    let cp = forced.checkpoint;
    let expected_work = recompute_work(&seq.engine, Fixed::ONE);
    assert_eq!((expected_work.0, expected_work.1), expected);
    let mut samples = Vec::new();
    for index in 0..216 {
        let checkpoint = cp.clone();
        let started = Instant::now();
        black_box(&seq).queue_checkpoint(checkpoint).unwrap();
        let elapsed = started.elapsed();
        if index >= 16 {
            samples.push(elapsed);
        }
        let captured = queued.try_recv().unwrap();
        assert_eq!((captured.liabilities, captured.deposits, captured.coverage), expected_work);
        let registered = registry.lock().unwrap();
        let registered = registered.get(&cp.seq).unwrap();
        assert_eq!((registered.liabilities, registered.deposits, registered.coverage), expected_work);
    }
    let mut scans = Vec::new();
    for _ in 0..7 {
        let started = Instant::now();
        let observed = recompute(black_box(&seq.engine), black_box(Fixed::ONE));
        scans.push(started.elapsed());
        assert_eq!(black_box(observed), expected);
    }
    samples.sort_unstable();
    scans.sort_unstable();
    println!(
        "ACCOUNTING migrated_accounts={count} current_held_deposits={count} capture_p50={:?} capture_p99={:?} scan_p50={:?}",
        samples[100], samples[198], scans[3]
    );
    let replay = recovery_capture_p50(&log_entries(&dir), expected);
    (samples[100], replay, scans[3])
}

/// Creates distinct deterministic receipt inputs for the local population fixture.
fn population_receipt(index: u64) -> DepositReceipt {
    DepositReceipt {
        block: 1,
        block_hash: B256::repeat_byte(1),
        tx_hash: format!("0x{index:064x}").parse().unwrap(),
        log_index: index,
        transaction_deposit_index: 0,
        deposit_id: index,
        beneficiary: DepositBeneficiary::Trader { account: account(index) },
        amount: Fixed::from_int(10),
    }
}

/// Applies one live command and checks its cache, forced checkpoint, and registered work.
fn apply(
    seq: &mut Sequencer,
    queued: &mut mpsc::Receiver<Work>,
    unpaid: &mut Fixed,
    cmd: Command,
) -> (Vec<Event>, Vec<Work>) {
    let events = seq.sequence(cmd).unwrap().expect("trusted stamp");
    *unpaid += approved(&events);
    assert_matches(seq.accounting, &seq.engine, *unpaid);
    let expected = recompute_work(&seq.engine, *unpaid);
    let mut work = Vec::new();
    while let Ok(captured) = queued.try_recv() {
        assert_eq!(captured.checkpoint.seq, seq.last_seq().unwrap());
        assert_eq!((captured.liabilities, captured.deposits, captured.coverage), expected);
        let registry = seq.gate.as_ref().unwrap().registry.as_ref().unwrap().lock().unwrap();
        let registered = registry.get(&captured.checkpoint.seq).unwrap();
        assert_eq!((registered.liabilities, registered.deposits, registered.coverage), expected);
        work.push(captured);
    }
    assert_eq!(work.len(), 1, "every applied command forces exactly one checkpoint");
    (events, work)
}

/// Advances both the host clock and its trusted sample.
fn move_to(seq: &mut Sequencer, clock: &TestClock, ts: Millis) {
    clock.advance(Duration::from_millis((ts - clock.now_ms()) as u64));
    seq.trust(sample_around(clock)).unwrap();
}

/// Quotes from two independent venues.
fn prices(market: u32, mid: &str, ts: Millis) -> Command {
    Command::Prices {
        market,
        quotes: ["venue-a", "venue-b"]
            .map(|source| SourceQuote { source: source.into(), mid: mid.parse().unwrap(), ts, contract: None })
            .to_vec(),
    }
}

/// A market order for the funded trader.
fn order(market: u32, side: Side, qty: &str, quoted: &str) -> Command {
    Command::PlaceOrder(OrderRequest {
        account: TRADER,
        client_id: 1,
        market,
        side,
        qty: qty.parse().unwrap(),
        kind: OrderKind::Market { quoted: quoted.parse().unwrap(), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    })
}

/// A distinct address for each benchmark account.
fn account(index: u64) -> AccountId {
    let mut address = [0; 20];
    address[12..].copy_from_slice(&index.to_be_bytes());
    AccountId(address)
}
