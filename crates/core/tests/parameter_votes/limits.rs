//! Parameter votes from `V56` at their limits: under `V56`, rotation held on by a drawn turn the engine
//! refused, a restriction lapsing at a boundary a later command walks, the caps on waiting changes and
//! restrictions in force, and a refused change whose command pays queued withdrawals after its outcome.

use super::*;

use engine_core::governance::{MAX_PENDING_CHANGES, MAX_RESTRICTIONS};
use engine_core::turns::{DAY_MS, boundary};

use super::rotation::{draw, house_cap, install, next_turn};

#[test]
fn under_v56_a_drawn_turn_the_engine_refused_keeps_votes_governing_from_its_mirror_until_an_installation() {
    let mut h = governed_under(ProtocolVersion::V56, TTL);
    // The set was mirrored a day before its first turn's draw, so the turn has no seven days' notice:
    // credited and refused.
    let turn = next_turn(&h);
    h.advance_to(boundary(turn).expect("in range") - DAY_MS);
    assert!(matches!(draw(&mut h, turn, 1).last(), Some(Event::TurnRefused { .. })));
    switch_rotation(&mut h, false);
    // The registry still has rotation on while the drawn turn names the operator.
    let raise = margin(&h, BTC, 1_500);
    rejected(&h.operate(raise.clone()), "only a passed vote changes this");
    assert!(pass(&mut h, 1, true, raise).iter().any(|e| matches!(e, Event::RestrictionApplied { .. })));
    install(&mut h);
    // The installation ends the drawn turns: the timelock governs, and the operator's listing applies.
    assert_eq!(h.operate(house_cap(&h, ETH, -1)).last(), Some(&Event::AdminApplied));
    let command = house_cap(&h, ETH, -2);
    refused(&pass(&mut h, 2, true, command), "rotation is off");
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

#[test]
fn a_refused_change_pays_queued_withdrawals_after_its_outcome() {
    // Under `V23`, whose client actions need no screening, a trader wins 3,000 on BTC from a house with
    // no capital, takes the physical cash, and queues 200 withdrawals of 10 behind it. The house then
    // pays in for all of them, and each command after pays at most 16.
    let mut h = Harness::zero_house_operated(ProtocolVersion::V23);
    let mut params = h.engine.params().unwrap().clone();
    params.house_total_cap_floor = f("10000000");
    h.admin(AdminCommand::SetParams(params));
    let btc = h.engine.markets()[&BTC].spec.clone();
    h.admin(AdminCommand::UpsertMarket(MarketSpec { house_cap_floor: f("1000000"), ..btc }));
    let trader = acct(1);
    h.deposit(trader, "10000");
    for (side, mid) in [(Side::Buy, "10000"), (Side::Sell, "13000")] {
        h.price(BTC, mid);
        let events = h.market_order(trader, BTC, side, "1", mid);
        assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
    }
    let events = h.cmd(Command::Withdraw { account: trader, amount: f("10000"), auth: None });
    assert!(events.iter().any(|e| matches!(e, Event::WithdrawalApproved { .. })), "{events:?}");
    for _ in 0..200 {
        h.cmd(Command::Withdraw { account: trader, amount: f("10"), auth: None });
    }
    assert_eq!(h.engine.withdrawal_queue().count(), 200);
    h.cmd(Command::HouseCapital { amount: f("2000"), chain_ref: "0xhouse-in".into() });
    // Staking, `V37`, the ceilings, `V59`, the vote contract, a set in effect and rotation on; twice the
    // launch profit fee passes, and fails its ±50% range when it applies.
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V25 });
    let day = 24 * 60 * 60 * 1000;
    let staking = engine_core::StakingConfig {
        liquidity_floor: f("1000000"),
        epoch_ms: day,
        start_ms: h.ts,
        cooldown_ms: 3 * day,
    };
    h.operator_admin(AdminCommand::InitStaking(staking));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V37 });
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 });
    h.admin(AdminCommand::SetGovernance(GovernanceConfig { vote: VOTE, restriction_ttl_ms: TTL }));
    let now = h.ts;
    super::rotation::adopt_set(&mut h, 1, now);
    switch_rotation(&mut h, true);
    let doubled = with_params(&h, |p| p.profit_fee_bps *= 2);
    let logged = h.ts;
    pass(&mut h, 1, false, doubled.clone());
    assert!(h.engine.withdrawal_queue().count() > 16, "requests are left for the change's command to pay");
    h.advance_to(logged + NOTICE_MS);
    let events = apply(&mut h, 1, doubled);
    let outcome = events.iter().position(|e| matches!(e, Event::AdminRejected { .. })).expect("refused");
    let paid = events[outcome + 1..].iter().filter(|e| matches!(e, Event::WithdrawalApproved { .. })).count();
    assert_eq!(paid, 16, "{events:?}");
    assert!(matches!(events.last(), Some(Event::WithdrawalQueueUpdated { .. })), "{events:?}");
}
