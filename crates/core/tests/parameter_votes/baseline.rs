//! Parameter votes from `V56` over a moving baseline: voted commands that meet their own checks, a
//! voted change applying only over the baseline it passed on, new parameters under a restriction,
//! renewal after the baseline moves, the stress levels a restriction holds, and the one waiting
//! ceiling adoption.

use super::*;

#[test]
fn a_voted_access_policy_and_activation_meet_their_own_checks() {
    let mut h = governed();
    let policy = engine_core::access::AccessPolicy { source: AccountId([0x5a; 20]), max_age_ms: 60_000 };
    let record = AdminCommand::SetAccessPolicy(policy.clone());
    let again =
        AdminCommand::SetAccessPolicy(engine_core::access::AccessPolicy { max_age_ms: 30_000, ..policy.clone() });
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V56 };
    for (proposal, command) in [(1, record.clone()), (2, again.clone()), (3, activate.clone())] {
        assert!(matches!(pass(&mut h, proposal, false, command).last(), Some(Event::ProposalScheduled { .. })));
    }
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 1, record).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.access_policy(), Some(&policy));
    rejected(&apply(&mut h, 2, again), "already recorded");
    // An activation by vote runs as one, and is refused as one: the version is not newer.
    rejected(&apply(&mut h, 3, activate), "is not newer than");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V56);
}

#[test]
fn voted_parameters_under_a_restriction_move_only_the_baseline() {
    let mut h = governed();
    let fee = with_params(&h, |p| p.profit_fee_bps = 1_200);
    pass(&mut h, 1, false, fee.clone());
    h.advance(NOTICE_MS - TTL / 2);
    let closeout = with_params(&h, |p| p.closeout_level_bps = 6_000);
    pass(&mut h, 2, true, closeout);
    h.advance(TTL / 2);
    // The fee change carries the baseline's close-out level: the restriction's stays in force.
    assert_eq!(apply(&mut h, 1, fee).last(), Some(&Event::AdminApplied));
    let params = h.engine.params().unwrap();
    assert_eq!((params.profit_fee_bps, params.closeout_level_bps), (1_200, 6_000));
    assert_eq!(h.engine.baseline_params().unwrap().closeout_level_bps, 5_000);
    h.advance(TTL / 2);
    h.cmd(Command::Tick);
    let params = h.engine.params().unwrap();
    assert_eq!((params.profit_fee_bps, params.closeout_level_bps), (1_200, 5_000));
}

#[test]
fn with_rotation_off_the_operators_parameters_are_measured_from_the_baseline() {
    let mut h = governed();
    let closeout = with_params(&h, |p| p.closeout_level_bps = 6_000);
    pass(&mut h, 1, true, closeout);
    switch_rotation(&mut h, false);
    // Parameters keeping the baseline's close-out level change no rate, so they need no notice.
    let resting = with_params(&h, |p| p.max_resting_orders = 50);
    assert_eq!(h.operate(resting).last(), Some(&Event::AdminApplied));
    let params = h.engine.params().unwrap();
    assert_eq!((params.closeout_level_bps, params.max_resting_orders), (6_000, 50));
    assert_eq!(h.engine.baseline_params().unwrap().closeout_level_bps, 5_000);
}

#[test]
fn a_voted_change_applies_only_over_the_baseline_it_passed_on() {
    let mut h = governed();
    let fee = with_params(&h, |p| p.profit_fee_bps = 1_200);
    let closeout = with_params(&h, |p| p.closeout_level_bps = 5_500);
    pass(&mut h, 1, false, fee.clone());
    pass(&mut h, 2, false, closeout.clone());
    let (wider, narrower) = (margin(&h, BTC, 1_500), margin(&h, BTC, 1_600));
    pass(&mut h, 3, false, wider.clone());
    pass(&mut h, 4, false, narrower.clone());
    let eth = margin(&h, ETH, 1_500);
    pass(&mut h, 5, false, eth.clone());
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 1, fee).last(), Some(&Event::AdminApplied));
    // The close-out change carries the fee before the first change: it would undo it.
    rejected(&apply(&mut h, 2, closeout), "changed after it passed");
    assert_eq!(apply(&mut h, 3, wider).last(), Some(&Event::AdminApplied));
    rejected(&apply(&mut h, 4, narrower), "changed after it passed");
    // Another market's change stands on its own.
    assert_eq!(apply(&mut h, 5, eth).last(), Some(&Event::AdminApplied));
    let params = h.engine.params().unwrap();
    assert_eq!((params.profit_fee_bps, params.closeout_level_bps), (1_200, 5_000));
    assert_eq!((margin_bps(&h, BTC), margin_bps(&h, ETH)), (one_band(1_500), one_band(1_500)));
}

#[test]
fn the_command_that_set_a_restriction_renews_it_after_the_baseline_moves() {
    let mut h = governed();
    let closeout = with_params(&h, |p| p.closeout_level_bps = 6_000);
    let to_baseline = closeout.clone();
    pass(&mut h, 1, false, to_baseline.clone());
    pass(&mut h, 2, true, closeout.clone());
    h.advance(NOTICE_MS - TTL / 2);
    pass(&mut h, 3, true, closeout.clone());
    h.advance(TTL / 2);
    assert_eq!(apply(&mut h, 1, to_baseline).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.baseline_params().unwrap().closeout_level_bps, 6_000);
    // The same command again renews the restriction, though over the new baseline it restricts nothing.
    let events = pass(&mut h, 4, true, closeout);
    assert_eq!(
        events.iter().find(|e| matches!(e, Event::RestrictionApplied { .. })),
        Some(&Event::RestrictionApplied { proposal: 4, expires_at: h.ts + TTL })
    );
    assert_eq!(h.engine.governance().restrictions.keys().copied().collect::<Vec<_>>(), [4]);
}

#[test]
fn parameters_removing_a_restricted_stress_level_are_refused() {
    let mut h = governed();
    let two_levels = with_params(&h, |p| {
        p.stress_levels.pop();
    });
    pass(&mut h, 2, false, two_levels.clone());
    h.advance(NOTICE_MS - TTL / 2);
    pass(&mut h, 1, true, AdminCommand::SetStressFloor { market: Some(BTC), level: 2 });
    h.advance(TTL / 2);
    rejected(&apply(&mut h, 2, two_levels), "restriction 1 holds a floor at stress level 2");
    assert_eq!(h.engine.params().unwrap().stress_levels.len(), 3);
    assert_eq!(level(&h, BTC), 2);
}

#[test]
fn a_voted_ceiling_adoption_waits_for_no_other() {
    let mut h = governed();
    let later = h.ts + 30 * NOTICE_MS / 7 * 7;
    let btc = AdminCommand::AdoptCloseSpreadCeiling {
        market: BTC,
        max_close_spread_bps: 18,
        effective_at: later,
        listing: None,
    };
    let eth =
        AdminCommand::AdoptCloseSpreadCeiling { market: ETH, max_close_spread_bps: 28, effective_at: 0, listing: None };
    pass(&mut h, 1, false, btc.clone());
    pass(&mut h, 2, false, eth.clone());
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 1, btc).last(), Some(&Event::AdminApplied));
    rejected(&apply(&mut h, 2, eth), "another close-spread ceiling adoption waits");
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&ETH], 30);
}
