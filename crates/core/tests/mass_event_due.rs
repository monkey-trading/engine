//! `Engine::mass_event_due`: whether a command at a stamp would carry the rollover or a MONKEY period
//! close in its own events, which the sequencer answers with a `Tick` at that stamp first. A distribution
//! epoch's close is held to `staking/mirror_frontier.rs`.

mod common;

use common::Harness;
use common::emissions::schedule;
use engine_core::command::Command;
use engine_core::event::Event;

#[test]
fn a_period_close_is_due_from_the_periods_end_until_a_command_closes_it() {
    let mut h = Harness::emitting(1_000);
    let end = schedule(&h).period_end(0);
    assert!(!h.engine.mass_event_due(end - 1));
    assert!(h.engine.mass_event_due(end));
    h.advance_to(end);
    assert!(h.engine.mass_event_due(h.ts));
    let events = h.cmd(Command::Tick);
    assert!(events.iter().any(|event| matches!(event, Event::EmissionPeriodClosed { .. })), "{events:?}");
    assert!(!h.engine.mass_event_due(h.ts));
}

#[test]
fn a_rollover_is_due_from_its_time_until_a_command_runs_it() {
    let mut h = Harness::v17();
    let rollover = h.engine.next_rollover().expect("a rollover is scheduled");
    assert!(!h.engine.mass_event_due(rollover - 1));
    assert!(h.engine.mass_event_due(rollover));
    h.advance_to(rollover);
    assert!(h.engine.mass_event_due(h.ts));
    h.cmd(Command::Tick);
    assert!(h.engine.next_rollover().is_some_and(|next| next > rollover));
    assert!(!h.engine.mass_event_due(h.ts));
}
