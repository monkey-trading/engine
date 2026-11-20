//! A start holds ordinary sequencing for a publisher snapshot only once a rollover is due before one
//! applies: at its first admitted stamp over an unpaid rollover, whose catch-up the snapshot must
//! precede, or at its next rollover. A log that has paid every rollover due sequences at once.

use super::benchmark_recovery::{DAY, FIRST, Fixture, START, charge, charges, recover_clock, trusted};
use super::*;
use crate::log_heads::{Head, Hold, Publisher};
use crate::trusted_time::tests::logs::Logs;

#[test]
fn a_start_holds_for_a_snapshot_only_over_an_unpaid_rollover() {
    // The log's first rollover is due at `FIRST`. Each case: when the node before the restart paid
    // it, if it did, when the restart's first stamp is admitted, and whether that start holds.
    let cases = [
        ("before-the-rollover", None, FIRST - 1, false),
        ("at-the-rollover", None, FIRST, true),
        ("after-a-paid-rollover", Some(FIRST + 1), FIRST + 60_000, false),
        ("two-rollovers-unpaid", None, FIRST + DAY + 1, true),
    ];
    for (name, paid_at, at, held) in cases {
        let mut f = Fixture::new(&format!("benchmark-start-{name}"), START, true, true);
        let mut expected = Vec::new();
        if let Some(paid_at) = paid_at {
            // A running node: trusted time renewed before the rollover and crossing it within its grace.
            f.advance_to(FIRST - 30_000);
            f.seq.trust(trusted(f.clock.as_ref())).unwrap();
            f.advance_to(paid_at);
            expected.push(charge("0.01"));
            assert_eq!(charges(&f.submit(Command::Tick)), expected, "{name}");
        }
        f.seq.flush().unwrap();
        f.advance_to(at);
        let Fixture { dir, seq, clock } = f;
        drop(seq);

        // A start as a node with `[benchmarks]` makes one, its head poster watching the hold.
        let (logs, _guard) = Logs::capture();
        let mut seq = recover_clock(&dir, clock.clone());
        let publisher = Publisher::for_test(watch::channel(Head::default()).0, Default::default());
        publisher.gate.open();
        let hold = publisher.hold.subscribe();
        seq.log_heads = Some(publisher);
        seq.wait_for_benchmarks().unwrap();
        seq.trust(trusted(clock.as_ref())).unwrap();
        let published = if held { Hold::BenchmarkSync } else { Hold::Live };
        assert_eq!(*hold.borrow(), published, "{name}: the first admitted stamp publishes the start's hold");
        let mut f = Fixture { dir, seq, clock };
        let before = f.balance();
        let tick = f.seq.sequence(Command::Tick).unwrap();
        assert_eq!((tick.is_none(), f.seq.benchmark_sync), (held, held), "{name}");
        assert_eq!(logs.warnings("benchmark_sync_pending"), usize::from(held), "{name}");
        if held {
            assert_eq!(f.balance(), before, "{name}: nothing is charged before the snapshot");
            let missed = if at > FIRST + DAY { 2 } else { 1 };
            let snapshot = f.snapshot(&[("2026-10-20", "0.015"), ("2026-10-21", "0.02003")][..missed]);
            let fresh = [charge("0.015"), charge("0.02003")][..missed].to_vec();
            assert_eq!(charges(&f.submit(snapshot)), fresh, "{name}: the catch-up runs on the snapshot's rates");
            expected.extend(fresh);
        } else {
            assert!(charges(&tick.expect("the start holds nothing")).is_empty(), "{name}");
        }
        // The snapshot ends the hold's episode; a start that holds nothing ends one an earlier start left.
        assert_eq!(logs.count("alert_resolved=\"benchmark_sync_pending\""), 1, "{name}");
        assert!(charges(&f.submit(Command::Tick)).is_empty(), "{name}");
        f.assert_public_replay(&expected);
    }
}

#[test]
fn a_start_that_holds_nothing_charges_its_next_rollover_only_after_a_snapshot() {
    // A start just before the rollover finds the log current. The rollover is charged at the rates of
    // a snapshot applied since the start: with none by then, it waits for one, as after a schedule
    // change, since the log's last may predate a change this start cannot see.
    for snapshot_first in [false, true] {
        let name = format!("benchmark-start-next-rollover-{snapshot_first}");
        let Fixture { dir, seq, clock } = Fixture::new(&name, START, true, true);
        drop(seq);
        clock.advance(Duration::from_millis((FIRST - 1_000 - START) as u64));
        let mut seq = recover_clock(&dir, clock.clone());
        seq.wait_for_benchmarks().unwrap();
        seq.trust(trusted(clock.as_ref())).unwrap();
        let mut f = Fixture { dir, seq, clock };
        assert!(charges(&f.submit(Command::Tick)).is_empty(), "{name}: the start holds nothing");
        let snapshot = f.snapshot(&[("2026-10-20", "0.015")]);
        if snapshot_first {
            assert!(f.submit(snapshot.clone()).contains(&Event::AdminApplied), "{name}");
        }
        f.advance_to(FIRST + 1);
        let tick = f.seq.sequence(Command::Tick).unwrap();
        assert_eq!(tick.is_none(), !snapshot_first, "{name}");
        let charged = match tick {
            Some(events) => charges(&events),
            None => charges(&f.submit(snapshot)),
        };
        assert_eq!(charged, [charge("0.015")], "{name}: the rollover is charged at the fresh rate");
        assert!(!f.seq.benchmark_sync, "{name}");
        f.assert_public_replay(&[charge("0.015")]);
    }
}
