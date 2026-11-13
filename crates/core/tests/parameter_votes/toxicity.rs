//! Parameter votes held to the toxicity score's bounds: a voted change of the score tiers needs no
//! notice but stays within ±50% of each launch tier, and a voted relisting keeps its market's
//! markout floor.

use super::*;
use engine_core::Fixed;

/// The baseline parameters with the two score tiers adding `low` and `high` bps.
fn tiers(h: &Harness, low: i64, high: i64) -> AdminCommand {
    with_params(h, |params| {
        params.toxicity.score_tiers[0].extra_spread_bps = low;
        params.toxicity.score_tiers[1].extra_spread_bps = high;
    })
}

#[test]
fn a_voted_score_tier_change_needs_no_notice_and_stays_within_half_of_launch() {
    let mut h = governed();
    let launch: Vec<i64> = h
        .engine
        .operator_bounds()
        .launch_score_tiers
        .as_ref()
        .expect("recorded at V54")
        .iter()
        .map(|tier| tier.extra_spread_bps)
        .collect();
    assert_eq!(launch, [2, 5]);
    let (outside, within) = (tiers(&h, 4, 5), tiers(&h, 3, 7));
    pass(&mut h, 1, false, outside.clone());
    pass(&mut h, 2, false, within.clone());
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 1, outside), "outside ±50% of its launch tier");
    assert_eq!(apply(&mut h, 2, within).last(), Some(&Event::AdminApplied));
    let applied: Vec<i64> =
        h.engine.params().expect("set").toxicity.score_tiers.iter().map(|tier| tier.extra_spread_bps).collect();
    assert_eq!(applied, [3, 7]);
    assert!(h.engine.operator_bounds().notice.is_none(), "no parameter notice was needed");
}

#[test]
fn a_voted_relisting_keeps_its_markets_markout_floor() {
    let mut h = governed();
    let spec = h.engine.baseline_spec(BTC).expect("listed");
    let floor = spec.markout_floor_bps.expect("V54's activation sets every floor");
    let moved = AdminCommand::UpsertMarket(MarketSpec { markout_floor_bps: Some(floor + Fixed::ONE), ..spec });
    pass(&mut h, 1, false, moved.clone());
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 1, moved), "markout floor is fixed");
}
