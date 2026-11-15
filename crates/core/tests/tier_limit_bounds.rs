//! The operator's bounds on `V66`'s request and order limits, through `Engine::apply`: a table keeps
//! each tier's resting-order limit and request rate named and at or below launch's, never falling
//! with the tier, and moves only as a seven-day notice names it. A voted table is held to the same
//! (`parameter_votes/volume_tiers.rs`). Before `V66` a table may name no limit.

mod common;

use common::Harness;
use common::session_keys::funded_at;
use common::spreads::{applied, refused};
use engine_core::command::AdminCommand;
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice};
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Millis;
use engine_core::volume_tier::{ACTIVATION_MAX_RESTING_ORDERS, ACTIVATION_REQUESTS_PER_SECOND, VolumeTier};

const V64: ProtocolVersion = ProtocolVersion::V64;
const V66: ProtocolVersion = ProtocolVersion::V66;

#[test]
fn a_table_naming_a_limit_above_launch_or_none_or_one_that_falls_is_refused() {
    let (mut h, _, _) = funded_at(V66);
    let in_force = h.engine.params().unwrap().volume_tiers.clone();
    let due = h.ts + NOTICE_MS;
    let cases: [(Change, &str); 7] = [
        (
            |tiers| tiers[0].max_resting_orders = 26,
            "max_resting_orders 26 must be named and at most its launch value 25",
        ),
        (
            |tiers| tiers[3].requests_per_second = 21,
            "requests_per_second 21 must be named and at most its launch value 20",
        ),
        (|tiers| tiers.iter_mut().for_each(|tier| tier.max_resting_orders = 0), "max_resting_orders 0 must be named"),
        (|tiers| tiers.iter_mut().for_each(|tier| tier.requests_per_second = 0), "requests_per_second 0 must be named"),
        (|tiers| tiers[3].max_resting_orders = 0, "max_resting_orders is named for every tier or none"),
        (|tiers| tiers[2].max_resting_orders = 40, "never falls with the tier"),
        (|tiers| tiers[1].requests_per_second = 1, "never falls with the tier"),
    ];
    for (change, reason) in cases {
        let mut tiers = in_force.clone();
        change(&mut tiers);
        let refusal = refused(&h.operate(notice(tiers, due))).to_owned();
        assert!(refusal.contains(reason), "{reason}: {refusal}");
    }
    // A limit above the maximum is no table at all.
    let mut tiers = in_force;
    tiers[3].max_resting_orders = 1_001;
    let refusal = refused(&h.operate(notice(tiers, due))).to_owned();
    assert!(refusal.contains("in 1..=1000"), "{refusal}");
}

#[test]
fn a_lower_limit_is_noticed_and_applies_from_its_time() {
    let (mut h, _, _) = funded_at(V66);
    let in_force = h.engine.params().unwrap().volume_tiers.clone();
    assert_eq!(in_force.iter().map(|tier| tier.max_resting_orders).collect::<Vec<_>>(), ACTIVATION_MAX_RESTING_ORDERS);
    assert_eq!(
        in_force.iter().map(|tier| tier.requests_per_second).collect::<Vec<_>>(),
        ACTIVATION_REQUESTS_PER_SECOND
    );
    let mut lower = in_force;
    lower.iter_mut().zip([10, 20, 40, 100]).for_each(|(tier, limit)| tier.max_resting_orders = limit);
    lower.iter_mut().zip([1, 2, 4, 8]).for_each(|(tier, rate)| tier.requests_per_second = rate);
    let set = AdminCommand::SetParams(params(&h, lower.clone()));
    assert!(refused(&h.operate(set.clone())).contains("needs a notice of at least seven days"));
    let due = h.ts + NOTICE_MS;
    assert!(applied(&h.operate(notice(lower.clone(), due))));
    h.advance_to(due);
    assert!(applied(&h.operate(set)));
    assert_eq!(h.engine.params().unwrap().volume_tiers, lower);
    // The launch table keeps the limits it started with, so the table cannot rise back above them.
    let launch = h.engine.operator_bounds().launch_volume_tiers.clone().unwrap();
    assert_eq!(launch.iter().map(|tier| tier.max_resting_orders).collect::<Vec<_>>(), ACTIVATION_MAX_RESTING_ORDERS);
    let mut raised = lower;
    raised[3].max_resting_orders = 251;
    let refusal = refused(&h.operate(notice(raised, h.ts + NOTICE_MS))).to_owned();
    assert!(refusal.contains("max_resting_orders 251 must be named and at most its launch value 250"), "{refusal}");
}

#[test]
fn before_v66_a_table_naming_limits_is_refused_whether_noticed_or_set() {
    let (mut h, _, _) = funded_at(V64);
    let mut named = h.engine.params().unwrap().volume_tiers.clone();
    named.iter_mut().zip(ACTIVATION_MAX_RESTING_ORDERS).for_each(|(tier, limit)| tier.max_resting_orders = limit);
    let reason = "a tier limit needs protocol V66";
    assert!(refused(&h.operate(notice(named.clone(), h.ts + NOTICE_MS))).contains(reason));
    assert!(refused(&h.operate(AdminCommand::SetParams(params(&h, named)))).contains(reason));
    let mut rates = h.engine.params().unwrap().volume_tiers.clone();
    rates.iter_mut().for_each(|tier| tier.requests_per_second = 2);
    assert!(refused(&h.operate(AdminCommand::SetParams(params(&h, rates)))).contains(reason));
}

// ── Helpers ──

/// An edit of a tier table.
type Change = fn(&mut Vec<VolumeTier>);

/// The parameters in force with `volume_tiers`.
fn params(h: &Harness, volume_tiers: Vec<VolumeTier>) -> ProtocolParams {
    ProtocolParams { volume_tiers, ..h.engine.params().unwrap().clone() }
}

/// A notice of `volume_tiers` from `effective_at`.
fn notice(volume_tiers: Vec<VolumeTier>, effective_at: Millis) -> AdminCommand {
    AdminCommand::AnnounceParams(ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress: None,
        score_tiers: None,
        profit_fee_curve: None,
        volume_tiers: Some(volume_tiers),
        effective_at,
    })
}
