//! Parameter votes from `V56` at their limits: rotation held on by a drawn turn the engine refused, a
//! restriction lapsing at a boundary a later command walks, and the caps on waiting changes and
//! restrictions in force.

use super::*;

use engine_core::election::RegistryInstallation;
use engine_core::emissions::TokenUnits;
use engine_core::governance::{MAX_PENDING_CHANGES, MAX_RESTRICTIONS};
use engine_core::turns::{RegistryRotationSet, RegistryTurn, SetMember, TurnReason, boundary};

#[test]
fn a_drawn_turn_the_engine_refused_keeps_votes_governing_until_an_installation() {
    let mut h = governed();
    let member =
        SetMember { candidate: AccountId([0xc1; 20]), admin_key: AccountId([0xa1; 20]), weight: TokenUnits(1) };
    let (block, chain_ref) = (h.log.len() as u64, "0xset:1".to_owned());
    let effective_at = h.ts + NOTICE_MS;
    let set =
        RegistryRotationSet { set: 1, epoch: 1, effective_at, quorum: true, members: vec![member], block, chain_ref };
    h.cmd(Command::AdoptRotationSet(set));
    // The set was mirrored just now, so its first turn has no seven days' notice: credited and refused.
    let turn = (0..).find(|turn| boundary(*turn).expect("in range") > h.ts).expect("a later turn");
    let (candidate, admin_key) = (member.candidate, member.admin_key);
    let (reason, block, chain_ref) = (TurnReason::Draw, h.log.len() as u64, "0xturn:1".to_owned());
    let drawn = RegistryTurn { turn, set: 1, position: 0, reason, candidate, admin_key, block, chain_ref };
    assert!(matches!(h.cmd(Command::AssignTurn(drawn)).last(), Some(Event::TurnRefused { .. })));
    switch_rotation(&mut h, false);
    // The registry still has rotation on while the drawn turn names the operator.
    let raise = margin(&h, BTC, 1_500);
    rejected(&h.operate(raise.clone()), "only a passed vote changes this");
    assert!(pass(&mut h, 1, true, raise).iter().any(|e| matches!(e, Event::RestrictionApplied { .. })));
    let (block, chain_ref) = (h.log.len() as u64, "0xinstall:1".to_owned());
    let installation = RegistryInstallation { candidate, admin_key, block, chain_ref };
    assert!(matches!(h.cmd(Command::InstallOperator(installation)).last(), Some(Event::OperatorInstalled { .. })));
    // The installation ends the drawn turns: the timelock governs, and the operator's listing applies.
    let spec = h.engine.baseline_spec(ETH).expect("listed");
    let cap = |bps| AdminCommand::UpsertMarket(MarketSpec { house_cap_bps: bps, ..spec.clone() });
    let (listing, restriction) = (cap(spec.house_cap_bps - 1), cap(spec.house_cap_bps - 2));
    assert_eq!(h.operate(listing).last(), Some(&Event::AdminApplied));
    refused(&pass(&mut h, 2, true, restriction), "rotation is off");
}

#[test]
fn a_restriction_lapses_at_a_walked_boundary_only_once_its_lifetime_has_run() {
    // A two-hour lifetime, so a restriction logged after the harness starts can end on either side of
    // the next rollover.
    let ttl = 2 * 3_600_000;
    for (expires_before, label) in [(true, "before the boundary"), (false, "after the boundary")] {
        let mut h = governed_with(ttl);
        let rollover = h.engine.next_rollover().expect("a rollover is scheduled");
        // The restriction ends an hour before the boundary, or half an hour after it.
        let expires_at = if expires_before { rollover - 3_600_000 } else { rollover + 1_800_000 };
        h.advance_to(expires_at - ttl);
        pass(&mut h, 1, true, AdminCommand::SetStressFloor { market: Some(BTC), level: 2 });
        h.advance_to(rollover + 3_600_000);
        let events = h.cmd(Command::Tick);
        let lapsed = events.iter().position(|e| *e == Event::RestrictionLapsed { proposal: 1 }).expect(label);
        let rolled = events.iter().position(|e| matches!(e, Event::BenchmarkRateMissing { .. })).expect(label);
        assert_eq!(lapsed < rolled, expires_before, "{label}: {events:?}");
    }
}

#[test]
fn the_waiting_changes_and_the_restrictions_in_force_are_capped() {
    let mut h = governed();
    for proposal in 0..MAX_PENDING_CHANGES as u64 {
        let command = margin(&h, BTC, 1_100 + proposal as i64);
        assert!(matches!(pass(&mut h, proposal, false, command).last(), Some(Event::ProposalScheduled { .. })));
    }
    let command = margin(&h, BTC, 2_000);
    refused(&pass(&mut h, 100, false, command), "64 changes already wait out their notice");

    let mut h = governed();
    for proposal in 0..MAX_RESTRICTIONS as u64 {
        let command = margin(&h, AAPL, 2_100 + proposal as i64);
        assert!(pass(&mut h, proposal, true, command).iter().any(|e| matches!(e, Event::RestrictionApplied { .. })));
    }
    let command = margin(&h, AAPL, 3_000);
    refused(&pass(&mut h, 100, true, command), "64 restrictions are already in force");
    // A renewal takes the place of the restriction it renews.
    let renewal = margin(&h, AAPL, 2_100);
    assert!(
        pass(&mut h, 101, true, renewal).iter().any(|e| matches!(e, Event::RestrictionApplied { proposal: 101, .. }))
    );
    assert_eq!(h.engine.governance().restrictions.len(), MAX_RESTRICTIONS);
}
