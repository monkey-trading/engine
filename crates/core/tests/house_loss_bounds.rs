//! The bounds on `V59`'s house loss trigger, through `Engine::apply`: a stress notice or the
//! operator's parameters keep each launch threshold at its level or above, at most its launch value,
//! after the seven-day notice; a lower threshold applies, and fires at once over an idle house; and
//! a threshold named before `V59` reads nothing until its activation keeps it. The votes' bound is in
//! `parameter_votes/house_loss.rs`.

mod common;

use common::house_loss::{harness, level, loss};
use common::session_keys::funded_at;
use common::{BTC, Harness, f};
use engine_core::Fixed;
use engine_core::command::AdminCommand;
use engine_core::event::Event;
use engine_core::house_loss::LossTrigger;
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice, StressSettings};
use engine_core::params::{ProtocolParams, StressLevel, StressTrigger};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Millis;

const KEPT: &str = "the stress levels keep a house loss trigger at level 2 or above, at most its launch threshold 0";

#[test]
fn a_stress_notice_keeps_the_threshold_at_its_level_and_at_most_launchs() {
    let (mut h, _) = harness(1_000, "100000");
    let due = h.ts + NOTICE_MS;
    let refusals = [
        ("dropped", with_threshold(&h, 2, None)),
        ("raised", with_threshold(&h, 2, Some(f("0.000000001")))),
        ("moved down a level", moved_to_level_one(&h)),
    ];
    for (case, params) in refusals {
        assert_eq!(refused(&h.operate(stress_notice(&params, due))), KEPT, "{case}");
    }
    // Kept, with a wider spread at level 2, a lower threshold, or another at a higher level, a notice
    // stands.
    let mut wider = with_threshold(&h, 2, Some(Fixed::ZERO));
    wider.stress_levels[2].min_spread_bps += 1;
    let mut higher = with_threshold(&h, 2, None);
    let zero = StressTrigger { house_loss: Some(Fixed::ZERO), ..StressTrigger::default() };
    higher.stress_levels.push(StressLevel {
        min_spread_bps: 7,
        trigger: Some(zero),
        ..higher.stress_levels[2].clone()
    });
    for params in [wider, higher, with_threshold(&h, 2, Some(f("-1000")))] {
        assert_eq!(h.operate(stress_notice(&params, due)).last(), Some(&Event::AdminApplied));
    }
}

#[test]
fn the_operators_parameters_keep_the_threshold_and_a_lower_one_applies_after_the_notice() {
    let (mut h, _) = harness(1_000, "100000");
    // Dropping the trigger is refused for the bound, before any notice is looked for.
    let dropped = with_threshold(&h, 2, None);
    assert_eq!(refused(&h.operate(AdminCommand::SetParams(dropped))), KEPT);

    // A threshold of -1,000 needs the notice, and from its time applies: an idle house has not earned
    // 1,000 over the day, so every priced market rises to level 2 at once.
    let lower = with_threshold(&h, 2, Some(f("-1000")));
    let due = h.ts + NOTICE_MS;
    let events = h.operate(AdminCommand::SetParams(lower.clone()));
    assert_eq!(refused(&events), "a stress parameter change needs a notice of at least seven days");
    h.operator_admin(stress_notice(&lower, due));
    h.advance_to(due);
    h.price(BTC, "100000");
    assert_eq!((loss(&h), level(&h, BTC)), (Fixed::ZERO, 0));
    let events = h.operate(AdminCommand::SetParams(lower));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert!(events.contains(&Event::HouseLossChanged { level: 2, loss: Fixed::ZERO }), "{events:?}");
    assert_eq!(level(&h, BTC), 2);
    // Launch stays the bound: a later notice may not go back above zero.
    let back = with_threshold(&h, 2, Some(f("1")));
    assert_eq!(refused(&h.operate(stress_notice(&back, h.ts + NOTICE_MS))), KEPT);
}

#[test]
fn before_v59_a_threshold_reads_nothing_and_activation_keeps_it() {
    // At `V48`, before stress parameters take a notice, the operator sets a threshold of -1,000 at
    // level 1: an idle house has not earned that over the day, yet nothing rises.
    let (mut h, _, _) = funded_at(ProtocolVersion::V48);
    let mut threshold = h.engine.params().expect("set").clone();
    threshold.stress_levels[1].trigger.as_mut().expect("level 1 has a trigger").house_loss = Some(f("-1000"));
    h.operator_admin(AdminCommand::SetParams(threshold.clone()));
    h.price(BTC, "100000");
    assert_eq!((h.engine.house_loss(), level(&h, BTC)), (None, 0));
    // `V59` keeps it as launch's, and from the command after the activation, which starts the
    // window, it fires.
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 });
    let launch = vec![LossTrigger { level: 1, house_loss: f("-1000") }];
    assert_eq!(h.engine.operator_bounds().launch_loss_triggers, Some(launch));
    let events = h.price(BTC, "100000");
    assert!(events.contains(&Event::HouseLossChanged { level: 1, loss: Fixed::ZERO }), "{events:?}");
    assert_eq!(level(&h, BTC), 1);
    // Any threshold within the input bound, either side of zero, is valid.
    for bound in [Fixed::MAX_INPUT, -Fixed::MAX_INPUT] {
        let mut p = threshold.clone();
        p.stress_levels[1].trigger.as_mut().expect("set above").house_loss = Some(bound);
        assert_eq!(p.validate(), Ok(()));
        let beyond = bound + if bound.is_positive() { Fixed::from_raw(1) } else { Fixed::from_raw(-1) };
        p.stress_levels[1].trigger.as_mut().expect("set above").house_loss = Some(beyond);
        assert_eq!(p.validate(), Err("stress level out of range".into()));
    }
}

// ── Helpers ──

/// The parameters in force with level `level`'s loss threshold at `threshold`.
fn with_threshold(h: &Harness, level: usize, threshold: Option<Fixed>) -> ProtocolParams {
    let mut params = h.engine.params().expect("set").clone();
    params.stress_levels[level].trigger.get_or_insert_with(StressTrigger::default).house_loss = threshold;
    params
}

/// The parameters in force with level 2's threshold moved to level 1.
fn moved_to_level_one(h: &Harness) -> ProtocolParams {
    let mut params = with_threshold(h, 2, None);
    params.stress_levels[1].trigger.as_mut().expect("level 1 has a trigger").house_loss = Some(Fixed::ZERO);
    params
}

fn stress_notice(params: &ProtocolParams, effective_at: Millis) -> AdminCommand {
    AdminCommand::AnnounceParams(ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress: Some(StressSettings::of(params)),
        score_tiers: None,
        profit_fee_curve: None,
        effective_at,
    })
}

fn refused(events: &[Event]) -> &str {
    match events.last() {
        Some(Event::AdminRejected { reason }) => reason,
        other => panic!("expected an admin rejection, got {other:?}"),
    }
}
