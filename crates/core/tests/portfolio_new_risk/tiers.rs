//! Earned leverage under portfolio margin (#913, D8 as revised on 17 November): each band of the initial
//! requirement keeps its tier's floor, so a lone position keeps its tier's leverage, while the credit on
//! matched exposure applies at every tier, so a hedged book may pass that leverage gross. `max_leverage`
//! keeps its meaning, a position opened alone, at every tier, hedged book or not.

use super::*;
use common::portfolio::params_with;
use common::weekend::{GOLD, OIL, genesis_with_params, globex};
use engine_core::params::ProtocolParams;
use engine_core::volume_tier::{VolumeTier, launch_volume_tiers};

#[test]
fn a_lone_position_keeps_its_tiers_leverage_and_a_hedged_book_passes_it_gross() {
    // Gold's bands are 1% to $2M; tier 0's floor is 5%, 20x. On $60k a lone $1.2M of gold is refused and
    // $1.18M fills.
    let mut h = tiered();
    let lone = client(&mut h, 1, "60000");
    let events = trade(&mut h, &lone, GOLD, Side::Buy, "600", "2000");
    let Reject::InsufficientMargin { required, .. } = rejection(&events) else { panic!("{events:?}") };
    assert_eq!(*required, f("60000"));
    only_fill(&trade(&mut h, &lone, GOLD, Side::Buy, "590", "2000"));
    // On the same $60k, $1M of gold against $900k of oil fills: about 32x gross.
    let hedged = client(&mut h, 2, "60000");
    only_fill(&trade(&mut h, &hedged, GOLD, Side::Buy, "500", "2000"));
    only_fill(&trade(&mut h, &hedged, OIL, Side::Sell, "12857.142857", "70"));
    let (equity, initial) = equity_and_initial(&h, &hedged);
    assert!(equity >= initial && initial < f("20000"), "{equity} against {initial}");
    assert_eq!(h.engine.volume_tier(hedged.account), 0);
}

#[test]
fn max_leverage_is_a_lone_positions_at_every_tier_hedged_or_not() {
    let mut h = tiered();
    let spec = h.engine.markets()[&GOLD].spec.clone();
    for tier in 0..4u8 {
        let a = client(&mut h, 10 + tier, "200000");
        // Each tier's threshold is a thousand dollars more: a lone BTC position held a minute counts it.
        if tier > 0 {
            only_fill(&trade(&mut h, &a, BTC, Side::Buy, &format!("0.0{tier}"), "100000"));
            h.advance(61_000);
            h.price(BTC, "100000");
            h.price(GOLD, "2000");
            h.price(OIL, "70");
        }
        assert_eq!(h.engine.volume_tier(a.account), usize::from(tier));
        let floor = TIER_FLOORS[usize::from(tier)];
        let expected = spec.max_leverage(floor);
        assert_eq!(h.engine.margin_standing(a.account).expect("credited").max_leverage[&GOLD], expected, "{tier}");
        only_fill(&trade(&mut h, &a, GOLD, Side::Buy, "500", "2000"));
        only_fill(&trade(&mut h, &a, OIL, Side::Sell, "12857.142857", "70"));
        assert_eq!(h.engine.margin_standing(a.account).expect("credited").max_leverage[&GOLD], expected, "{tier}");
    }
}

// ── Helpers ──

/// The margin floor of each tier `tiered`'s table names: 5%, 2%, then none.
const TIER_FLOORS: [i64; 4] = [500, 200, 0, 0];

/// A `V84` log under the test grid whose tier table reaches tier `n` at $1,000 × n of counted volume, with
/// `TIER_FLOORS`; gold, oil, BTC and ETH priced.
fn tiered() -> Harness {
    let tiers: Vec<VolumeTier> = (launch_volume_tiers().into_iter().enumerate())
        .map(|(n, tier)| VolumeTier {
            threshold: Fixed::from_int(1_000 * n as i64),
            day_divisor: 1,
            margin_floor_bps: TIER_FLOORS[n],
            ..tier
        })
        .collect();
    let params = ProtocolParams { volume_tiers: tiers, ..params_with(pairs(100)) };
    let mut h = genesis_with_params(V84, globex(vec![]), 750, 0, params);
    h.price(BTC, "100000");
    h.price(ETH, "4000");
    h.price(GOLD, "2000");
    h.price(OIL, "70");
    h
}
