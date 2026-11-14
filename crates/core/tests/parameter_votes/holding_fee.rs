//! Parameter votes held to the holding-time fee's bounds: a voted profit fee curve needs no notice
//! but stays at or above its launch curve at every hold and within the curve's longest hold, a voted
//! base fee never falls below its launch value, and from `V57` a voted curve reaching further lends an
//! add no more than the launch curve's last hold of age.

use super::*;
use common::close_spread::trade;
use common::events::only_fill;
use common::session_keys::funded_at;
use engine_core::holding_fee::{CurvePoint, MAX_CURVE_HOLD_MS};

/// The baseline parameters with the curve's second point at `fee_bps`.
fn second_point(h: &Harness, fee_bps: i64) -> AdminCommand {
    with_params(h, |params| params.profit_fee_curve[1].fee_bps = fee_bps)
}

#[test]
fn a_voted_curve_needs_no_notice_and_stays_at_or_above_launch() {
    let mut h = governed();
    let launch = h.engine.operator_bounds().launch_curve.clone().expect("recorded at V55");
    let (raised, below) = (second_point(&h, launch[1].fee_bps + 50), second_point(&h, launch[1].fee_bps - 50));
    let longer =
        with_params(&h, |params| params.profit_fee_curve.last_mut().expect("a curve").hold_ms = MAX_CURVE_HOLD_MS + 1);
    for (proposal, command) in [(1, below.clone()), (2, longer.clone()), (3, raised.clone())] {
        assert!(matches!(pass(&mut h, proposal, false, command).last(), Some(Event::ProposalScheduled { .. })));
    }
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 1, below), "below its launch curve");
    rejected(&apply(&mut h, 2, longer), "reaches at most");
    assert_eq!(apply(&mut h, 3, raised).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.params().expect("set").profit_fee_curve[1].fee_bps, launch[1].fee_bps + 50);
    assert!(h.engine.operator_bounds().notice.is_none(), "no parameter notice was needed");
}

#[test]
fn a_voted_base_fee_never_falls_below_launch() {
    let mut h = governed();
    let launch = h.engine.operator_bounds().launch.expect("recorded").profit_fee_bps;
    let cut = with_params(&h, |params| params.profit_fee_bps = launch - 1);
    pass(&mut h, 1, false, cut.clone());
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 1, cut), "below its launch value");
}

#[test]
fn a_voted_curve_extension_lends_an_add_no_more_than_the_launch_curves_last_hold() {
    // A declared client holds a BTC long under the latest version with rotation on and a set in
    // effect, and a vote extends the curve to 30 minutes with a flat 10% tail. The vote moves the curve
    // in force, not the launch curve, so a ninefold add to the long, held a week, counts the units held
    // as 15 minutes old: the open time moves 90 s back, not the 180 s the voted curve's reach would give.
    let (mut h, client, _) = funded_at(ProtocolVersion::LATEST);
    let config = engine_core::StakingConfig {
        liquidity_floor: f("1000000"),
        epoch_ms: 24 * 60 * 60 * 1000,
        start_ms: h.ts,
        cooldown_ms: 3 * 24 * 60 * 60 * 1000,
    };
    h.operator_admin(AdminCommand::InitStaking(config));
    h.admin(AdminCommand::SetGovernance(GovernanceConfig { vote: VOTE, restriction_ttl_ms: TTL }));
    let now = h.ts;
    super::rotation::adopt_set(&mut h, 1, now);
    switch_rotation(&mut h, true);
    let launch = h.engine.operator_bounds().launch_curve.clone();
    let extended = with_params(&h, |params| {
        params.profit_fee_curve.push(CurvePoint { hold_ms: MAX_CURVE_HOLD_MS, fee_bps: 1_000 })
    });
    assert!(matches!(pass(&mut h, 1, false, extended.clone()).last(), Some(Event::ProposalScheduled { .. })));
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "0.1", "100000"));
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 1, extended).last(), Some(&Event::AdminApplied));
    assert_eq!(
        h.engine.params().expect("set").profit_fee_curve.last().map(|point| point.hold_ms),
        Some(MAX_CURVE_HOLD_MS)
    );
    assert_eq!(h.engine.operator_bounds().launch_curve, launch, "a vote leaves the launch curve");
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "0.9", "100000"));
    assert_eq!(h.engine.accounts()[&client.account].opened_at.get(&BTC), Some(&(h.ts - 90_000)));
}
