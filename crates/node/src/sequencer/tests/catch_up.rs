//! A command stamped long after the engine's last rollover reaches its caller applied: the
//! sequencer sequences the catch-up batches the engine defers it for first.

use engine_core::corporate::MAX_CATCH_UP_BOUNDARIES;

use super::benchmark_recovery::{FIRST, Fixture, START, trusted};
use super::*;

const LOG_START: Millis = 1_792_504_800_000;
const DAY: Millis = 86_400_000;

#[tokio::test]
async fn a_command_after_a_long_outage_is_applied_once_the_catch_up_is_sequenced() {
    let dir = scratch_dir("seq-catch-up");
    let genesis = || genesis_commands(&dev_config().genesis);
    let node = TestSequencer::start_at(&dir, 64, Arc::new(TestClock::at(LOG_START)), genesis());
    let events = node.submit(deposit(100, "dep-0")).await;
    assert!(events.iter().any(|event| matches!(event, Event::DepositHeld { .. })), "{events:?}");
    node.stop().await;

    // Restart after an outage of more than one catch-up batch of daily rollovers.
    let after = LOG_START + (MAX_CATCH_UP_BOUNDARIES as Millis + 40) * DAY;
    let node = TestSequencer::start_at(&dir, 64, Arc::new(TestClock::at(after)), genesis());
    let events = node.submit(deposit(50, "dep-1")).await;
    assert!(!events.iter().any(|event| matches!(event, Event::CatchUpDeferred { .. })), "{events:?}");
    assert!(events.iter().any(|event| matches!(event, Event::DepositHeld { .. })), "{events:?}");
    node.stop().await;

    // The log holds the ticks that finished the catch-up, then the command.
    let logged = commands(&log_entries(&dir));
    let deposits = logged.iter().filter(|command| matches!(command, Command::Deposit { .. })).count();
    assert_eq!(deposits, 2, "no deferred attempt is logged: {logged:?}");
    assert!(logged.iter().any(|command| matches!(command, Command::Tick)), "{logged:?}");
    assert!(matches!(logged.last(), Some(Command::Deposit { .. })));
    let stamps: Vec<_> = log_entries(&dir)
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(c) => Some((c.sequenced.ts, matches!(c.sequenced.cmd, Command::Tick))),
            _ => None,
        })
        .collect();
    let last = stamps.last().unwrap().0;
    assert!(stamps.iter().filter(|(_, tick)| *tick).all(|(ts, _)| *ts < last), "ticks precede the command's stamp");
    replay(&log_entries(&dir)).unwrap();
}

#[test]
fn a_snapshot_after_a_long_outage_applies_first_and_a_rejected_one_catches_up_nothing() {
    let mut f = Fixture::new("seq-catch-up-snapshot", START, true, true);
    f.submit(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V33 }));
    let before = (f.seq.engine.next_rollover(), f.balance());
    f.advance_to(FIRST + (MAX_CATCH_UP_BOUNDARIES as Millis + 40) * DAY);
    f.seq.trust(trusted(f.clock.as_ref())).unwrap();

    // A snapshot the engine rejects leaves the backlog alone: no tick, no financing.
    let mut stale = f.batch(&[("2026-10-20", "0.015")]);
    stale.as_of = "2026-10-19".parse().unwrap();
    let rejected = f.submit(f.signed(stale, 0x42, 60_000));
    assert!(rejected.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{rejected:?}");
    assert!(!commands(&log_entries(&f.dir)).iter().any(|command| matches!(command, Command::Tick)));
    assert_eq!((f.seq.engine.next_rollover(), f.balance()), before);
    assert!(f.seq.benchmark_sync);

    // An applied one reports its outcome, releases the gate, and the ticks finish the catch-up after it.
    let applied = f.submit(f.snapshot(&[]));
    assert!(applied.contains(&Event::AdminApplied), "{applied:?}");
    assert!(!f.seq.benchmark_sync);
    // The ticks leave at most one batch for the next command, which is then not deferred.
    let next = f.submit(Command::Tick);
    assert!(!next.iter().any(|event| matches!(event, Event::CatchUpDeferred { .. })), "{next:?}");
    assert!(f.seq.engine.next_rollover().unwrap() > f.clock.true_ms());
    let logged = commands(&log_entries(&f.dir));
    let snapshot = logged.iter().rposition(|command| matches!(command, Command::SignedAdmin(_))).unwrap();
    assert!(logged[snapshot + 1..].iter().all(|command| matches!(command, Command::Tick)), "{logged:?}");
    assert!(logged.len() > snapshot + 1);
    f.seq.flush().unwrap();
    let mut replayed = replay(&log_entries(&f.dir)).unwrap();
    assert_eq!(replayed.engine.state_root(), f.seq.engine.state_root());
}
