//! Parameter votes held to the volume tier table's bounds: the vote that activates `V59` gives the
//! parameters the launch table, a voted table needs no notice but stays no more generous than
//! launch, and a voted relisting keeps each tier's spread within its launch spread to twice it.

use super::*;
use engine_core::Fixed;
use engine_core::spread_tier::SpreadTier;
use engine_core::volume_tier::launch_volume_tiers;

/// `governed`, with BTC, ETH and AAPL relisted before their launch bands are recorded at 1bp to $1m,
/// 2bp to $5m and 5bp above: a first band a share cut or a demotion can charge twice. At `governed`'s
/// free first band no table less generous than launch is within the bound.
fn governed_priced_from_the_first_dollar() -> Harness {
    let mut h = Harness::v21();
    for market in [BTC, ETH, AAPL] {
        let spread_tiers = vec![
            SpreadTier::bps(Some(Fixed::from_int(1_000_000)), 1),
            SpreadTier::bps(Some(Fixed::from_int(5_000_000)), 2),
            SpreadTier::bps(None, 5),
        ];
        h.operator_admin(AdminCommand::UpsertMarket(MarketSpec {
            spread_tiers,
            ..h.engine.markets()[&market].spec.clone()
        }));
    }
    governed_from(h, TTL)
}

#[test]
fn a_voted_table_needs_no_notice_and_stays_no_more_generous_than_launch() {
    let mut h = governed_priced_from_the_first_dollar();
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 };
    pass(&mut h, 1, false, activate.clone());
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 1, activate).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.params().expect("set").volume_tiers, launch_volume_tiers());
    let lower = with_params(&h, |params| params.volume_tiers[1].threshold = f("999999"));
    let wider = with_params(&h, |params| params.volume_tiers[3].best_price_share_bps = 30_000);
    let raised = with_params(&h, |params| params.volume_tiers[1].threshold = f("2000000"));
    for (proposal, command) in [(2, lower.clone()), (3, wider.clone()), (4, raised.clone())] {
        assert!(matches!(pass(&mut h, proposal, false, command).last(), Some(Event::ProposalScheduled { .. })));
    }
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 2, lower), "stay at or above launch's");
    rejected(&apply(&mut h, 3, wider), "is above its launch value");
    assert_eq!(apply(&mut h, 4, raised).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.params().expect("set").volume_tiers[1].threshold, f("2000000"));
    assert!(h.engine.operator_bounds().notice.is_none(), "no parameter notice was needed");
    // A voted relisting at 4bp to $5m is twice launch's 2bp for the market alone, but an account at
    // tier 1 at launch, now at tier 0, pays it from $250k where it paid 1bp to $500k.
    let mut spread_tiers = h.engine.operator_bounds().launch_bands[&BTC].spread_tiers.clone();
    spread_tiers[1] = SpreadTier::tenths(spread_tiers[1].up_to, 2 * spread_tiers[1].tenth_bps());
    let relisting =
        AdminCommand::UpsertMarket(MarketSpec { spread_tiers, ..h.engine.baseline_spec(BTC).expect("listed") });
    pass(&mut h, 5, false, relisting.clone());
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 5, relisting), "market 1 at volume tier 0, for an account at tier 1 at launch");
}
