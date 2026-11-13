//! Parameter votes held to the holding-time fee's bounds: a voted profit fee curve needs no notice
//! but stays at or above its launch curve at every hold and within the curve's longest hold, and a
//! voted base fee never falls below its launch value.

use super::*;
use engine_core::holding_fee::MAX_CURVE_HOLD_MS;

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
