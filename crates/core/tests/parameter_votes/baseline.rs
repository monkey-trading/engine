//! Parameter votes from `V56` over a moving baseline: voted commands that meet their own checks, a
//! voted change applying only over the baseline it passed on, also when mirrored under a restriction,
//! new parameters under a restriction, renewal after the baseline moves, restrictions lowering the
//! house caps and their floors, the stress levels a restriction holds, and the one waiting ceiling
//! adoption.

use super::*;

#[test]
fn a_voted_access_policy_and_activation_meet_their_own_checks() {
    let mut h = governed();
    let policy = engine_core::access::AccessPolicy { source: AccountId([0x5a; 20]), max_age_ms: 60_000 };
    let record = AdminCommand::SetAccessPolicy(policy.clone());
    let again =
        AdminCommand::SetAccessPolicy(engine_core::access::AccessPolicy { max_age_ms: 30_000, ..policy.clone() });
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 };
    for (proposal, command) in [(1, record.clone()), (2, again.clone()), (3, activate.clone())] {
        assert!(matches!(pass(&mut h, proposal, false, command).last(), Some(Event::ProposalScheduled { .. })));
    }
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 1, record).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.access_policy(), Some(&policy));
    rejected(&apply(&mut h, 2, again), "already recorded");
    // An activation by vote runs as one, and is refused as one: the version is not newer.
    rejected(&apply(&mut h, 3, activate), "is not newer than");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V59);
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
fn a_normal_vote_mirrored_under_a_restriction_that_lapses_before_it_applies_moves_the_baseline() {
    for market in [true, false] {
        let mut h = governed();
        let (restriction, voted) = if market {
            (margin(&h, ETH, 1_500), margin(&h, ETH, 1_100))
        } else {
            (with_params(&h, |p| p.closeout_level_bps = 6_000), with_params(&h, |p| p.profit_fee_bps = 1_200))
        };
        pass(&mut h, 1, true, restriction);
        // Mirrored over the baseline, which the restriction leaves in place beneath the values in force.
        pass(&mut h, 2, false, voted.clone());
        h.advance(TTL);
        let events = h.cmd(Command::Tick);
        assert!(events.contains(&Event::RestrictionLapsed { proposal: 1 }), "{events:?}");
        h.advance(NOTICE_MS - TTL);
        assert_eq!(apply(&mut h, 2, voted).last(), Some(&Event::AdminApplied), "market {market}");
        let params = h.engine.params().unwrap();
        let expected = if market { (one_band(1_100), 1_000, 5_000) } else { (one_band(1_000), 1_200, 5_000) };
        assert_eq!((margin_bps(&h, ETH), params.profit_fee_bps, params.closeout_level_bps), expected);
    }
}

#[test]
fn a_normal_vote_mirrored_under_a_restriction_renewed_through_its_application_survives_the_lapse() {
    // Restrictions last the seven days a normal change waits, so one renewal holds through the apply.
    let mut h = governed_with(NOTICE_MS);
    let restriction = margin(&h, ETH, 1_500);
    pass(&mut h, 1, true, restriction.clone());
    let voted = margin(&h, ETH, 1_100);
    pass(&mut h, 2, false, voted.clone());
    h.advance(NOTICE_MS - TTL);
    let events = pass(&mut h, 3, true, restriction);
    assert!(events.contains(&Event::RestrictionApplied { proposal: 3, expires_at: h.ts + NOTICE_MS }), "{events:?}");
    h.advance(TTL);
    assert_eq!(apply(&mut h, 2, voted).last(), Some(&Event::AdminApplied));
    assert_eq!(
        (margin_bps(&h, ETH), h.engine.baseline_spec(ETH).unwrap().margin_tiers),
        (one_band(1_500), one_band(1_100))
    );
    h.advance(NOTICE_MS - TTL);
    let events = h.cmd(Command::Tick);
    assert!(events.contains(&Event::RestrictionLapsed { proposal: 3 }), "{events:?}");
    assert_eq!(margin_bps(&h, ETH), one_band(1_100), "the voted baseline survives the lapse");
}

#[test]
fn fast_path_restrictions_lower_a_markets_house_cap_and_the_cap_floors_until_they_lapse() {
    // With rotation off the operator sets floors a restriction can lower; then votes govern.
    let mut h = voting(ProtocolVersion::V59, TTL);
    let spec = h.engine.baseline_spec(ETH).unwrap();
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { house_cap_floor: f("1000000"), ..spec }));
    let total_floor = with_params(&h, |p| p.house_total_cap_floor = f("5000000"));
    h.operator_admin(total_floor);
    let now = h.ts;
    super::rotation::adopt_set(&mut h, 1, now);
    switch_rotation(&mut h, true);
    let market = |h: &Harness, cap: i64, floor: &str| {
        let spec = h.engine.baseline_spec(ETH).unwrap();
        AdminCommand::UpsertMarket(MarketSpec { house_cap_bps: cap, house_cap_floor: f(floor), ..spec })
    };
    let total = |h: &Harness, floor: &str| with_params(h, |p| p.house_total_cap_floor = f(floor));
    let in_force = |h: &Harness| {
        let spec = &h.engine.markets()[&ETH].spec;
        (spec.house_cap_bps, spec.house_cap_floor, h.engine.params().unwrap().house_total_cap_floor)
    };
    let command = market(&h, 4_000, "400000");
    pass(&mut h, 1, true, command);
    let command = total(&h, "2000000");
    pass(&mut h, 2, true, command);
    assert_eq!(in_force(&h), (4_000, f("400000"), f("2000000")));
    // A second pair half a lifetime later: the lower cap and floors of each hold.
    h.advance(TTL / 2);
    let command = market(&h, 3_000, "700000");
    pass(&mut h, 3, true, command);
    let command = total(&h, "3000000");
    pass(&mut h, 4, true, command);
    assert_eq!(in_force(&h), (3_000, f("400000"), f("2000000")));
    h.advance(TTL / 2);
    h.cmd(Command::Tick);
    assert_eq!(in_force(&h), (3_000, f("700000"), f("3000000")), "the first pair lapsed");
    h.advance(TTL / 2);
    h.cmd(Command::Tick);
    assert_eq!(in_force(&h), (5_000, f("1000000"), f("5000000")), "the baseline returns");
    assert!(h.engine.governance().market_baselines.is_empty() && h.engine.governance().params_baseline.is_none());
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
