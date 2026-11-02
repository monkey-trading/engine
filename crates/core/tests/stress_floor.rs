//! The operator's stress floor from `V46`, through `Engine::apply`: at most level 1, in a market or
//! in all of them, lapsing 72 hours of sequenced time after it is set, with a higher floor in force
//! at activation dropping to level 1 and the automatic triggers and reduce-only unchanged.

mod common;

use common::close_spread::{ceilings, install_expect};
use common::{BTC, ETH, Harness};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::params::{ProtocolParams, StressLevel};
use engine_core::protocol::ProtocolVersion;
use engine_core::stress::STRESS_FLOOR_LAPSE_MS;
use engine_core::types::Millis;

/// The parameters in force with a third level, so a floor above level 1 names one that exists.
fn three_levels(h: &Harness) -> ProtocolParams {
    let mut parameters = h.engine.params().expect("the fixture sets parameters").clone();
    let top = StressLevel { min_spread_bps: 10, trigger: None, ..parameters.stress_levels[1].clone() };
    parameters.stress_levels.push(top);
    parameters
}

/// The fixture at `version`, with three stress levels in force.
fn at(version: ProtocolVersion) -> Harness {
    let mut h = Harness::v21();
    let parameters = three_levels(&h);
    h.operator_admin(AdminCommand::SetParams(parameters));
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version });
    h
}

fn floor(market: Option<u32>, level: usize) -> AdminCommand {
    AdminCommand::SetStressFloor { market, level }
}

fn level(h: &Harness, market: u32) -> usize {
    h.engine.markets()[&market].stress.level
}

/// The effective level of BTC and ETH.
fn levels(h: &Harness) -> (usize, usize) {
    (level(h, BTC), level(h, ETH))
}

#[test]
fn a_floor_above_level_1_is_refused_for_one_market_or_for_all() {
    for market in [Some(BTC), None] {
        let mut h = at(ProtocolVersion::V46);
        let events = h.operate(floor(market, 2));
        assert_eq!(
            events.last(),
            Some(&Event::AdminRejected { reason: "the operator's stress floor is at most level 1".into() })
        );
        assert_eq!(levels(&h), (0, 0));
        assert_eq!(h.operate(floor(market, 1)).last(), Some(&Event::AdminApplied));
        assert_eq!(level(&h, BTC), 1);
    }
    // A level that does not exist keeps its own refusal.
    let mut h = at(ProtocolVersion::V46);
    let events = h.operate(floor(Some(BTC), 3));
    assert_eq!(events.last(), Some(&Event::AdminRejected { reason: "stress level 3 does not exist".into() }));
}

#[test]
fn a_floor_lapses_72_hours_after_it_is_set_on_the_sequencers_stamp() {
    for (market, expected) in [(Some(BTC), (1, 0)), (None, (1, 1))] {
        let mut h = at(ProtocolVersion::V46);
        h.operator_admin(floor(market, 1));
        assert_eq!(levels(&h), expected);
        let set = h.ts;

        h.advance_to(set + STRESS_FLOOR_LAPSE_MS - 1);
        h.cmd(Command::Tick);
        assert_eq!(levels(&h), expected, "held a millisecond before");

        h.advance_to(set + STRESS_FLOOR_LAPSE_MS);
        let events = h.cmd(Command::Tick);
        assert_eq!(levels(&h), (0, 0), "lapsed at the time");
        assert!(events.contains(&Event::StressChanged { market: BTC, level: 0 }), "{events:?}");
        assert_eq!(h.engine.markets()[&BTC].stress.floor, 0);
        assert_eq!(h.engine.markets()[&BTC].stress.floor_set_at, None);
    }
}

#[test]
fn a_rollover_a_catch_up_walks_before_the_lapse_is_priced_with_the_floor() {
    // No command falls between the floor and its lapse: a Tick at the lapse walks the rollovers
    // between them, and only then does the floor go.
    let mut h = at(ProtocolVersion::V46);
    h.operator_admin(floor(Some(BTC), 1));
    let lapses_at = h.ts + STRESS_FLOOR_LAPSE_MS;
    assert!(h.engine.next_rollover().unwrap() < lapses_at);
    h.advance_to(lapses_at);
    let events = h.cmd(Command::Tick);
    let lapsed = events.iter().position(|event| *event == Event::StressChanged { market: BTC, level: 0 });
    let rolled = events.iter().rposition(|event| {
        matches!(
            event,
            Event::BenchmarkRateMissing { .. }
                | Event::BenchmarkRateObserved { .. }
                | Event::BenchmarkRateUndated { .. }
        )
    });
    assert!(rolled.is_some() && lapsed.is_some() && rolled < lapsed, "{events:?}");
}

#[test]
fn setting_a_floor_again_restarts_its_time() {
    let mut h = at(ProtocolVersion::V46);
    h.operator_admin(floor(Some(BTC), 1));
    let set = h.ts;
    let renewed = set + STRESS_FLOOR_LAPSE_MS - 1;
    h.advance_to(renewed);
    h.operator_admin(floor(Some(BTC), 1));

    h.advance_to(set + STRESS_FLOOR_LAPSE_MS);
    h.cmd(Command::Tick);
    assert_eq!(level(&h, BTC), 1, "the first time no longer counts");
    h.advance_to(renewed + STRESS_FLOOR_LAPSE_MS - 1);
    h.cmd(Command::Tick);
    assert_eq!(level(&h, BTC), 1);
    h.advance_to(renewed + STRESS_FLOOR_LAPSE_MS);
    h.cmd(Command::Tick);
    assert_eq!(level(&h, BTC), 0);
}

#[test]
fn clearing_a_floor_stops_its_time_and_one_market_lapses_alone() {
    let mut h = at(ProtocolVersion::V46);
    h.operator_admin(floor(Some(BTC), 1));
    h.operator_admin(floor(Some(BTC), 0));
    assert_eq!(h.engine.markets()[&BTC].stress.floor_set_at, None);
    assert_eq!(level(&h, BTC), 0);

    // ETH's floor is set later than BTC's, and lapses later.
    h.operator_admin(floor(Some(BTC), 1));
    let set = h.ts;
    h.advance_to(set + 1_000);
    h.operator_admin(floor(Some(ETH), 1));
    h.advance_to(set + STRESS_FLOOR_LAPSE_MS);
    h.cmd(Command::Tick);
    assert_eq!(levels(&h), (0, 1));
    h.advance_to(set + 1_000 + STRESS_FLOOR_LAPSE_MS);
    h.cmd(Command::Tick);
    assert_eq!(levels(&h), (0, 0));
}

#[test]
fn a_global_floor_and_a_market_floor_keep_their_own_times() {
    let mut h = at(ProtocolVersion::V46);
    h.operator_admin(floor(None, 1));
    let set = h.ts;
    h.advance_to(set + 1_000);
    h.operator_admin(floor(Some(BTC), 1));
    h.advance_to(set + STRESS_FLOOR_LAPSE_MS);
    h.cmd(Command::Tick);
    assert_eq!(levels(&h), (1, 0), "the global floor lapses, and BTC's still holds");
    h.advance_to(set + 1_000 + STRESS_FLOOR_LAPSE_MS);
    h.cmd(Command::Tick);
    assert_eq!(levels(&h), (0, 0));
}

#[test]
fn a_floor_above_level_1_at_activation_drops_to_level_1_and_starts_its_time() {
    for (market_floor, global_floor, expected) in [(2, 0, (1, 0)), (1, 0, (1, 0)), (0, 2, (1, 1)), (2, 1, (1, 1))] {
        let mut h = at(ProtocolVersion::V45);
        h.operator_admin(floor(Some(BTC), market_floor));
        h.operator_admin(floor(None, global_floor));
        assert_eq!(level(&h, BTC), market_floor.max(global_floor));
        h.advance(100_000);
        let activated = h.ts;
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V46 });
        assert_eq!(levels(&h), expected, "{market_floor} {global_floor}");
        // Nothing lapses yet, though the floors were set long before the activation.
        h.advance_to(activated + STRESS_FLOOR_LAPSE_MS - 1);
        h.cmd(Command::Tick);
        assert_eq!(levels(&h), expected, "{market_floor} {global_floor}");
        h.advance_to(activated + STRESS_FLOOR_LAPSE_MS);
        h.cmd(Command::Tick);
        assert_eq!(levels(&h), (0, 0), "{market_floor} {global_floor}");
    }
}

#[test]
fn earlier_versions_take_any_level_and_never_lapse() {
    let mut h = at(ProtocolVersion::V45);
    h.operator_admin(floor(Some(BTC), 2));
    assert_eq!(h.engine.markets()[&BTC].stress.floor_set_at, None);
    let set = h.ts;
    h.advance_to(set + 2 * STRESS_FLOOR_LAPSE_MS);
    h.cmd(Command::Tick);
    assert_eq!(level(&h, BTC), 2);
}

#[test]
fn the_automatic_triggers_reach_higher_levels_and_a_lapse_leaves_them() {
    let mut h = at(ProtocolVersion::V46);
    // A move that meets level 1's trigger now reaches the top level by itself.
    let mut parameters = h.engine.params().unwrap().clone();
    parameters.stress_levels[2].trigger = parameters.stress_levels[1].trigger.clone();
    let move_bps = parameters.stress_levels[2].trigger.as_ref().unwrap().move_bps.unwrap();
    h.operator_admin(AdminCommand::SetParams(parameters));
    h.operator_admin(floor(Some(BTC), 1));
    let set = h.ts;
    h.price(BTC, "100000");
    let moved = (100_000 + 100_000 * move_bps / 10_000 + 1).to_string();
    h.price(BTC, &moved);
    assert_eq!(h.engine.markets()[&BTC].stress.auto, 2);
    assert_eq!(level(&h, BTC), 2);

    // Past the floor's 72 hours, the cooldown and the move window, nothing holds the market up.
    h.advance_to(set + STRESS_FLOOR_LAPSE_MS);
    h.price(BTC, &moved);
    assert_eq!(h.engine.markets()[&BTC].stress.floor, 0);
    assert_eq!(h.engine.markets()[&BTC].stress.auto, 0);
    assert_eq!(level(&h, BTC), 0);
}

#[test]
fn reduce_only_does_not_lapse() {
    let mut h = at(ProtocolVersion::V46);
    h.operator_admin(AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true });
    let set = h.ts;
    h.advance_to(set + 2 * STRESS_FLOOR_LAPSE_MS);
    h.cmd(Command::Tick);
    assert!(h.engine.markets()[&BTC].reduce_only);
}

#[test]
fn a_replay_reaches_the_same_root_across_a_lapse() {
    let mut h = at(ProtocolVersion::V46);
    h.operator_admin(floor(Some(BTC), 1));
    h.operator_admin(floor(None, 1));
    let set: Millis = h.ts;
    h.cmd(Command::Tick);
    let held = h.engine.state_root();
    assert_eq!(common::batch::replayed(&h).state_root(), held);
    h.advance_to(set + STRESS_FLOOR_LAPSE_MS);
    h.cmd(Command::Tick);
    let lapsed = h.engine.state_root();
    assert_ne!(lapsed, held);
    assert_eq!(common::batch::replayed(&h).state_root(), lapsed);
}
