//! From `V33` one command catches up on at most `MAX_CATCH_UP_BOUNDARIES` rollovers and corporate
//! boundaries before its own stamp; a command stamped further on is deferred and later commands
//! continue, in the same chronological order and with the same charges as an unbounded walk.

use engine_core::Fixed;
use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate};
use engine_core::command::{AdminCommand, Command};
use engine_core::corporate::MAX_CATCH_UP_BOUNDARIES;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};

use crate::common::*;
use crate::support::*;

const DAY: Millis = 86_400_000;

/// `held("2", "0")` with financing charged each day, under `V33` when `bounded`.
fn financed(bounded: bool) -> Harness {
    let mut h = held("2", "0");
    let mut spec = h.engine.markets()[&AAPL].spec.clone();
    spec.financing.markup_bps = 3650;
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    if bounded {
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V33 });
    }
    h
}

fn financings(events: &[Event]) -> usize {
    events.iter().filter(|e| matches!(e, Event::Financing { account, .. } if *account == acct(1))).count()
}

fn deferred(events: &[Event]) -> Option<Millis> {
    events.iter().find_map(|e| match e {
        Event::CatchUpDeferred { due } => Some(*due),
        _ => None,
    })
}

#[test]
fn a_far_past_stamp_catches_up_a_bounded_batch_and_defers_the_command() {
    let mut h = financed(true);
    let first_due = h.engine.next_rollover().unwrap();
    // Two thousand days past the last rollover, where an unbounded walk would charge them all.
    h.advance_to(first_due + 2_000 * DAY);
    let events = h.market_order(acct(1), AAPL, Side::Sell, "1", "80");
    assert_eq!(financings(&events), MAX_CATCH_UP_BOUNDARIES, "{events:?}");
    let due = deferred(&events).expect("the command is deferred");
    assert_eq!(h.engine.next_rollover(), Some(due));
    assert!(due < h.ts);
    // Nothing else of the command ran: no order was accepted, and the position is unchanged.
    let reached_the_order =
        |e: &Event| matches!(e, Event::OrderAccepted { .. } | Event::OrderRejected { .. } | Event::Fill(_));
    assert!(!events.iter().any(reached_the_order), "{events:?}");
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).qty, f("2"));
}

#[test]
fn deferred_batches_charge_exactly_what_an_unbounded_walk_charges() {
    let mut bounded = financed(true);
    let mut unbounded = financed(false);
    let first_due = bounded.engine.next_rollover().unwrap();
    assert_eq!(first_due, unbounded.engine.next_rollover().unwrap());
    let target = first_due + 1_200 * DAY;

    unbounded.advance_to(target);
    let walked = unbounded.cmd(Command::Tick);
    assert!(deferred(&walked).is_none());
    let days = financings(&walked);
    assert!(days > 3 * MAX_CATCH_UP_BOUNDARIES);

    bounded.advance_to(target);
    let (mut charged, mut commands) = (0, 0);
    loop {
        let events = bounded.cmd(Command::Tick);
        charged += financings(&events);
        commands += 1;
        if deferred(&events).is_none() {
            break;
        }
    }
    assert_eq!(charged, days);
    assert!((days / MAX_CATCH_UP_BOUNDARIES..=days / MAX_CATCH_UP_BOUNDARIES + 1).contains(&commands), "{commands}");
    assert_eq!(bounded.balance(acct(1)), unbounded.balance(acct(1)));
    assert_eq!(bounded.engine.next_rollover(), unbounded.engine.next_rollover());
    bounded.assert_conserved(Fixed::ZERO);
}

#[test]
fn a_command_within_the_bound_is_not_deferred() {
    let mut h = financed(true);
    let first_due = h.engine.next_rollover().unwrap();
    h.advance_to(first_due + (MAX_CATCH_UP_BOUNDARIES as Millis - 2) * DAY);
    let events = h.cmd(Command::Tick);
    assert!(deferred(&events).is_none(), "{events:?}");
    assert!(financings(&events) >= MAX_CATCH_UP_BOUNDARIES - 2);
    assert!(h.engine.next_rollover().unwrap() > h.ts);
}

#[test]
fn before_v33_a_far_past_stamp_walks_every_missed_rollover() {
    let mut h = financed(false);
    let first_due = h.engine.next_rollover().unwrap();
    h.advance_to(first_due + 2_000 * DAY);
    let events = h.cmd(Command::Tick);
    assert!(deferred(&events).is_none());
    assert!(financings(&events) > 1_900, "{}", financings(&events));
    assert!(h.engine.next_rollover().unwrap() > h.ts);
}

/// The rollover and dividend events for `acct(1)` of every command until one is not deferred.
fn economic_order(h: &mut Harness) -> Vec<&'static str> {
    let mut order = vec![];
    loop {
        let events = h.cmd(Command::Tick);
        order.extend(events.iter().filter_map(|e| match e {
            Event::Financing { account, .. } if *account == acct(1) => Some("rollover"),
            Event::CorporateDividend { account, .. } if *account == acct(1) => Some("dividend"),
            _ => None,
        }));
        if deferred(&events).is_none() {
            return order;
        }
    }
}

#[test]
fn a_corporate_action_inside_a_deferred_gap_applies_in_chronological_order() {
    let schedule_later = |h: &mut Harness| {
        let mut later = dividend("year-on", AAPL, "1", "0");
        later.previous_close = ny(2027, 11, 19, 16, 0);
        later.ex_open = ny(2027, 11, 22, 9, 30);
        accepted(&schedule(h, later));
        h.advance_to(ny(2028, 1, 14, 12, 0));
    };
    // The dividend goes ex more than a year on, past the first batch, and lands between the same
    // rollovers as in an unbounded walk.
    let (mut bounded, mut unbounded) = (financed(true), financed(false));
    schedule_later(&mut bounded);
    schedule_later(&mut unbounded);
    let order = economic_order(&mut bounded);
    assert_eq!(order, economic_order(&mut unbounded));
    let at = order.iter().position(|e| *e == "dividend").expect("the dividend applied");
    assert!(at > MAX_CATCH_UP_BOUNDARIES, "applied after the first batch: {at}");
    assert!(bounded.engine.corporate_actions().is_empty());
    assert_eq!(bounded.balance(acct(1)), unbounded.balance(acct(1)));
}

#[test]
fn the_boundary_at_the_commands_own_stamp_is_extra() {
    let mut h = financed(true);
    let first_due = h.engine.next_rollover().unwrap();
    // Exactly MAX boundaries precede the stamp, one rollover falls on it: not deferred, all charged.
    let stamp = (1..=MAX_CATCH_UP_BOUNDARIES).fold(first_due, |at, _| {
        let rollover = h.engine.params().unwrap().rollover.clone();
        rollover.next_after(at)
    });
    h.advance_to(stamp);
    let events = h.cmd(Command::Tick);
    assert!(deferred(&events).is_none(), "{events:?}");
    assert_eq!(financings(&events), MAX_CATCH_UP_BOUNDARIES + 1);
    // One more day and the command is deferred.
    let mut h = financed(true);
    h.advance_to(stamp + DAY);
    assert!(deferred(&h.cmd(Command::Tick)).is_some());
}

#[test]
fn a_benchmark_batch_applied_before_its_deferral_reports_its_outcome() {
    let mut h = financed(true);
    let first_due = h.engine.next_rollover().unwrap();
    h.advance_to(first_due + 2_000 * DAY);
    let batch = BenchmarkBatch {
        as_of: h.engine.benchmark_date(h.ts),
        checked: ["SOFR".to_owned()].into(),
        rates: vec![BenchmarkRate {
            benchmark: "SOFR".into(),
            effective_date: h.engine.benchmark_date(h.ts),
            annual_rate: f("0.04"),
        }],
    };
    let events = h.operate(AdminCommand::SetBenchmarkRates(batch));
    assert!(deferred(&events).is_some(), "{events:?}");
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
}

#[test]
fn catch_up_pending_matches_the_stamps_the_engine_defers() {
    let first_due = financed(true).engine.next_rollover().unwrap();
    let mut later = dividend("pending-split", AAPL, "1", "0");
    later.previous_close = ny(2026, 10, 30, 16, 0);
    later.ex_open = ny(2026, 11, 2, 9, 30);
    for with_action in [false, true] {
        // Rollovers fall every day, so each day past the first adds one boundary; the action adds two.
        for days in [0, 1, 300, 364, 365, 366, 367, 900] {
            let mut h = financed(true);
            if with_action {
                accepted(&schedule(&mut h, later.clone()));
            }
            let ts = first_due + days * DAY;
            let pending = h.engine.catch_up_pending(ts);
            h.advance_to(ts);
            let events = h.cmd(Command::Tick);
            assert_eq!(pending, deferred(&events).is_some(), "{days} days, action {with_action}");
        }
    }
    assert!(!held("2", "0").engine.catch_up_pending(first_due + 900 * DAY), "before V33 nothing is deferred");
}
