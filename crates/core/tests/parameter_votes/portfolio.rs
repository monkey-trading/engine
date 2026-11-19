//! Votes on `V81`'s offset grid: a normal vote moves its shocks and floor shares within their launch
//! bounds with no notice, the fast path raises a floor share at once, in force as the stricter of the
//! baseline and every restriction until it lapses, and a vote carries a relisting of several markets,
//! which the operator alone may not send while votes govern.

use super::*;
use common::portfolio::{group, member};
use engine_core::portfolio::PortfolioGrid;

/// BTC and ETH as one group at a floor share of `share`, which may rise to all of it.
fn crypto(share: i64) -> PortfolioGrid {
    PortfolioGrid { groups: vec![group("crypto", share, vec![member(BTC, "1", 20), member(ETH, "1", 20)])] }
}

/// `governed_under(V81)`, the parameters naming `crypto(5000)` before the activation takes it up.
fn governed_grid() -> Harness {
    let mut h = Harness::v21();
    let params = ProtocolParams { portfolio_grid: crypto(5_000), ..h.engine.params().expect("set").clone() };
    h.operator_admin(AdminCommand::SetParams(params));
    governed_from(h, ProtocolVersion::V81, TTL)
}

/// The floor share of the crypto group in force, and in the baseline.
fn shares(h: &Harness) -> (i64, i64) {
    let baseline = h.engine.baseline_params().expect("set").portfolio_grid.groups[0].floor_share_bps;
    (h.engine.params().expect("set").portfolio_grid.groups[0].floor_share_bps, baseline)
}

#[test]
fn a_fast_path_floor_share_applies_at_once_and_lapses_at_its_lifetime() {
    let mut h = governed_grid();
    assert_eq!(h.engine.operator_bounds().launch_grid, Some(crypto(5_000)));
    let logged = h.ts;
    let raise = with_params(&h, |params| params.portfolio_grid.groups[0].floor_share_bps = 7_000);
    let events = pass(&mut h, 1, true, raise);
    assert!(events.contains(&Event::RestrictionApplied { proposal: 1, expires_at: logged + TTL }), "{events:?}");
    assert_eq!(shares(&h), (7_000, 5_000));
    let floor_shares = Some([("crypto".to_owned(), 7_000)].into());
    assert!(
        h.engine
            .governance()
            .restrictions
            .values()
            .any(|held| matches!(&held.limit, Limit::Params { floor_shares: f, .. } if *f == floor_shares)),
        "the restriction holds the floor share"
    );
    // A lower restriction, a second later, leaves the stricter one in force.
    h.advance(1_000);
    let lower = with_params(&h, |params| params.portfolio_grid.groups[0].floor_share_bps = 6_000);
    pass(&mut h, 2, true, lower);
    assert_eq!(shares(&h), (7_000, 5_000));
    h.advance_to(logged + TTL);
    let events = h.cmd(Command::Tick);
    assert!(events.contains(&Event::RestrictionLapsed { proposal: 1 }), "{events:?}");
    assert_eq!(shares(&h), (6_000, 5_000), "the later restriction holds until its own lifetime");
}

#[test]
fn a_fast_path_proposal_lowering_a_floor_share_or_moving_a_shock_is_refused() {
    let mut h = governed_grid();
    let lower = with_params(&h, |params| params.portfolio_grid.groups[0].floor_share_bps = 4_000);
    refused(&pass(&mut h, 1, true, lower), "lowers the close-out level or a floor share");
    let shock = with_params(&h, |params| {
        params.portfolio_grid.groups[0].members[0].residual_shock_bps = 30;
    });
    refused(&pass(&mut h, 2, true, shock), "changes another parameter");
    assert_eq!(shares(&h), (5_000, 5_000));
}

#[test]
fn a_normal_vote_moves_the_grid_within_its_launch_bounds_with_no_notice() {
    let mut h = governed_grid();
    let raised = with_params(&h, |params| {
        params.portfolio_grid.groups[0].members[1].residual_shock_bps = 60;
        params.portfolio_grid.groups[0].floor_share_bps = 8_000;
    });
    let lowered = with_params(&h, |params| params.portfolio_grid.groups[0].members[1].residual_shock_bps = 10);
    let reloaded = with_params(&h, |params| params.portfolio_grid.groups[0].members[1].loading = f("1.2"));
    for (proposal, command) in [(1, raised.clone()), (2, lowered.clone()), (3, reloaded.clone())] {
        assert!(matches!(pass(&mut h, proposal, false, command).last(), Some(Event::ProposalScheduled { .. })));
    }
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 2, lowered), "residual shock 10 must be from its launch value 20");
    rejected(&apply(&mut h, 3, reloaded), "fixed at its launch grid");
    assert_eq!(apply(&mut h, 1, raised).last(), Some(&Event::AdminApplied));
    let grid = &h.engine.params().expect("set").portfolio_grid.groups[0];
    assert_eq!((grid.members[1].residual_shock_bps, grid.floor_share_bps), (60, 8_000));
    assert!(h.engine.operator_bounds().notice.is_none(), "no parameter notice was needed");
}

#[test]
fn a_vote_carries_a_relisting_of_several_markets_and_the_operator_cannot_send_one() {
    let mut h = governed_grid();
    let relisted =
        |id: MarketId, h: &Harness| MarketSpec { max_slippage_bps: 75, ..h.engine.baseline_spec(id).expect("listed") };
    let relisting = AdminCommand::RelistMarkets { specs: vec![relisted(BTC, &h), relisted(ETH, &h)] };
    rejected(&h.operate(relisting.clone()), "only a passed vote");
    assert!(matches!(pass(&mut h, 1, false, relisting.clone()).last(), Some(Event::ProposalScheduled { .. })));
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 1, relisting).last(), Some(&Event::AdminApplied));
    assert_eq!(
        (h.engine.markets()[&BTC].spec.max_slippage_bps, h.engine.markets()[&ETH].spec.max_slippage_bps),
        (75, 75)
    );
}
