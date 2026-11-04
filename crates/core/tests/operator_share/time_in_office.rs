//! `V50`: the operator's share split by time in office. Each command credits the time since the last
//! one to the account in office over it, before its own handover; a close splits its fifth by the time
//! the open epoch counted, and the time no one served and the rounding go to stakers.

use std::collections::BTreeMap;

use engine_core::staking::TimeInOffice;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

use super::*;

const HOUR_MS: Millis = 60 * 60 * 1000;

/// `at` under `V50` with rotation on, counting from the harness's start, which is epoch 0's.
fn rotating_v50(excess: u128) -> Harness {
    let mut h = at(ProtocolVersion::V50, excess);
    switch(&mut h, true);
    h
}

/// Every operator's part among `events`, in event order.
fn shares(events: &[Event]) -> Vec<(AccountId, u128)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::OperatorShareAllocated { account, amount, .. } => Some((*account, amount.0)),
            _ => None,
        })
        .collect()
}

/// The excess a close measured, in USDC base units.
fn excess(events: &[Event]) -> u128 {
    events
        .iter()
        .find_map(|event| match event {
            Event::DistributionEpochClosed { excess, .. } => Some(excess.0),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no epoch closed: {events:?}"))
}

/// When epoch `n` ends.
fn end_of(h: &Harness, n: u64) -> Millis {
    h.engine.staking().expect("staking is initialised").config.epoch_end(n)
}

/// The open epoch's time in office.
fn office(h: &Harness) -> TimeInOffice {
    h.engine.staking().and_then(|staking| staking.office.clone()).expect("V50 counts time in office")
}

/// `fifth · served / counted`, rounded down.
fn part(fifth: u128, served: Millis, counted: Millis) -> u128 {
    fifth * served as u128 / counted as u128
}

#[test]
fn a_handover_halfway_through_an_epoch_splits_the_fifth_between_the_two_operators() {
    let mut h = rotating_v50(1_000_000_000);
    let start = end_of(&h, 7);
    elect(&mut h, 1, CANDIDATE, key(), start + 12 * HOUR_MS);
    // Nothing is staked at the close that opens the epoch, so it pays no one and starts the count.
    h.advance_to(start);
    assert_eq!(allocated(&h.cmd(Command::Tick)), UsdcUnits(0));
    assert_eq!(office(&h), TimeInOffice { since: start, served: Default::default() });
    stake(&mut h, STAKER_A, 1);

    h.advance_to(start + 12 * HOUR_MS);
    finalised(&mut h, start + 12 * HOUR_MS);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
    let events = close(&mut h);
    let mut expected = vec![(operator().address, 100_000_000), (CANDIDATE, 100_000_000)];
    expected.sort();
    assert_eq!(shares(&events), expected);
    assert_eq!(
        [leaf(&h, operator().address), leaf(&h, CANDIDATE), leaf(&h, STAKER_A)],
        [100_000_000, 100_000_000, 800_000_000]
    );
    assert_eq!(allocated(&events), UsdcUnits(1_000_000_000));
    assert_eq!(office(&h), TimeInOffice { since: end_of(&h, 8), served: Default::default() });
}

#[test]
fn each_part_rounds_down_and_the_rounding_goes_to_stakers() {
    // A third of the day to the outgoing operator and two thirds to the incoming one: each part loses
    // a fraction of a unit, which the only staker takes.
    let mut h = rotating_v50(1_000_000_000);
    let start = end_of(&h, 7);
    elect(&mut h, 1, CANDIDATE, key(), start + 8 * HOUR_MS);
    h.advance_to(start);
    h.cmd(Command::Tick);
    stake(&mut h, STAKER_A, 1);
    h.advance_to(start + 8 * HOUR_MS);
    finalised(&mut h, start + 8 * HOUR_MS);
    h.cmd(Command::Tick);
    let events = close(&mut h);
    let parts: BTreeMap<AccountId, u128> = shares(&events).into_iter().collect();
    assert_eq!(parts, BTreeMap::from([(operator().address, 66_666_666), (CANDIDATE, 133_333_333)]));
    assert_eq!(leaf(&h, STAKER_A), 800_000_001);
    assert_eq!(allocated(&events), UsdcUnits(1_000_000_000));
}

#[test]
fn a_handover_in_the_closes_own_command_leaves_that_epoch_to_the_outgoing_operator() {
    // The handover is due at the epoch's end or just after it, in the command that closes it: the
    // epoch's time was served before it, so the outgoing operator earns all of it, and the incoming one
    // earns the next epoch whole.
    for after_the_end in [0, 1] {
        let mut h = rotating_v50(1_000_000_000);
        stake(&mut h, STAKER_A, 1);
        let end = end_of(&h, 8);
        let effective_at = end + after_the_end;
        elect(&mut h, 1, CANDIDATE, key(), effective_at);
        // The close before opens the epoch the handover ends, paying the key in office throughout.
        h.advance_to(end_of(&h, 7));
        assert_eq!(shares(&h.cmd(Command::Tick)), vec![(operator().address, 200_000_000)]);
        earn(&mut h);
        h.advance_to(end - 1);
        finalised(&mut h, effective_at);
        assert_eq!(h.engine.operator().map(|operator| operator.address), Some(operator().address));

        h.advance_to(effective_at);
        let events = h.cmd(Command::Tick);
        assert_eq!(h.engine.operator().map(|operator| operator.address), Some(key()));
        assert_eq!(shares(&events), vec![(operator().address, excess(&events) / 5)], "{after_the_end}");
        assert_eq!(leaf(&h, CANDIDATE), 0);

        earn(&mut h);
        let events = close(&mut h);
        assert_eq!(shares(&events), vec![(CANDIDATE, excess(&events) / 5)], "{after_the_end}");
    }
}

#[test]
fn skipped_epochs_count_inside_the_open_epoch_for_whoever_held_the_office() {
    let mut h = rotating_v50(1_000_000_000);
    stake(&mut h, STAKER_A, 1);
    let handover = end_of(&h, 7) + 6 * HOUR_MS;
    elect(&mut h, 1, CANDIDATE, key(), handover);

    // The first close after seven unwitnessed epochs pays the operator in office through all of them
    // the whole fifth.
    h.advance_to(end_of(&h, 7));
    let events = h.cmd(Command::Tick);
    assert!(events.contains(&Event::DistributionEpochsSkipped { first: 0, last: 6 }), "{events:?}");
    assert_eq!(shares(&events), vec![(operator().address, 200_000_000)]);

    // Six hours in, the office changes; then nothing is sequenced for three days, which the incoming
    // operator held.
    earn(&mut h);
    h.advance_to(handover);
    finalised(&mut h, handover);
    h.cmd(Command::Tick);
    let close_at = end_of(&h, 10) + 12 * HOUR_MS;
    h.advance_to(close_at);
    let events = h.cmd(Command::Tick);
    assert!(events.contains(&Event::DistributionEpochsSkipped { first: 8, last: 9 }), "{events:?}");
    let (fifth, counted) = (excess(&events) / 5, close_at - end_of(&h, 7));
    let parts: BTreeMap<AccountId, u128> = shares(&events).into_iter().collect();
    let expected = BTreeMap::from([
        (operator().address, part(fifth, 6 * HOUR_MS, counted)),
        (CANDIDATE, part(fifth, close_at - handover, counted)),
    ]);
    assert_eq!(parts, expected);
    assert!(allocated(&events).0 <= excess(&events), "{events:?}");
}

#[test]
fn an_epoch_with_no_time_in_office_pays_stakers_alone() {
    // Rotation off throughout: no one is in office.
    let mut h = at(ProtocolVersion::V50, 1_000_000_000);
    stake(&mut h, STAKER_A, 1);
    let events = close(&mut h);
    assert_eq!((shares(&events), leaf(&h, STAKER_A)), (vec![], 1_000_000_000));
    assert_eq!(leaf(&h, operator().address), 0);

    // Rotation turned on in the close's own command: the close runs first, over an epoch no one served.
    earn(&mut h);
    let staking = h.engine.staking().unwrap();
    h.advance_to(staking.config.epoch_end(staking.config.epochs_elapsed(h.ts)));
    let events = switch(&mut h, true);
    assert!(matches!(events.last(), Some(Event::RotationSwitched { enabled: true, .. })), "{events:?}");
    assert!(excess(&events) > 0 && shares(&events).is_empty(), "{events:?}");
    assert_eq!(leaf(&h, operator().address), 0);

    // From then on the operator serves the whole next epoch, and earns the whole fifth.
    earn(&mut h);
    let events = close(&mut h);
    assert_eq!(shares(&events), vec![(operator().address, excess(&events) / 5)]);
}

#[test]
fn rotation_off_for_part_of_the_epoch_leaves_that_part_to_stakers() {
    let mut h = rotating_v50(1_000_000_000);
    stake(&mut h, STAKER_A, 1);
    h.advance(6 * HOUR_MS);
    switch(&mut h, false);
    let events = close(&mut h);
    // Six hours of the day in office: a quarter of the fifth, and the rest of the excess to the staker.
    assert_eq!(shares(&events), vec![(operator().address, 50_000_000)]);
    assert_eq!(leaf(&h, STAKER_A), 950_000_000);
    assert_eq!(allocated(&events), UsdcUnits(1_000_000_000));
}

#[test]
fn a_close_that_distributes_nothing_still_restarts_the_count() {
    // The deployer key serves seven epochs with nothing staked, which the frontier's command closes;
    // that time is not carried into the next epoch, which the incoming operator serves alone.
    let mut h = rotating_v50(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, key());
    assert_eq!(office(&h), TimeInOffice { since: h.ts, served: Default::default() });
    assert_eq!(h.engine.staking().unwrap().distributed, UsdcUnits(0));
    h.cmd(Command::Tick);
    stake(&mut h, STAKER_A, 1);
    let events = close(&mut h);
    assert_eq!(shares(&events), vec![(CANDIDATE, 200_000_000)]);
    assert_eq!(leaf(&h, operator().address), 0);
}

#[test]
fn v50_starts_counting_at_its_activation_and_the_close_it_runs_with_keeps_the_old_rule() {
    let mut h = rotating(1_000_000_000);
    stake(&mut h, STAKER_A, 1);
    let end = end_of(&h, 7);
    elect(&mut h, 1, CANDIDATE, key(), end - 6 * HOUR_MS);
    h.advance_to(end_of(&h, 6));
    h.cmd(Command::Tick);
    earn(&mut h);
    h.advance_to(end - 6 * HOUR_MS);
    finalised(&mut h, end - 6 * HOUR_MS);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
    assert_eq!(h.engine.staking().unwrap().office, None);

    // The epoch the activation's own command closes pays under `V47`: all to the candidate in office
    // at the close, though the deployer key held the office for three quarters of it.
    h.advance_to(end);
    let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V50 }));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(shares(&events), vec![(CANDIDATE, excess(&events) / 5)]);
    assert_eq!(office(&h), TimeInOffice { since: end, served: Default::default() });

    // Activated six hours into the next epoch, the count starts there: the key in effect serves all of it.
    let mut h = rotating(1_000_000_000);
    stake(&mut h, STAKER_A, 1);
    h.advance(6 * HOUR_MS);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V50 });
    let events = close(&mut h);
    assert_eq!(shares(&events), vec![(operator().address, 200_000_000)]);
}

#[test]
fn an_activation_from_before_v47_seats_the_candidate_already_in_effect_before_counting() {
    // Under `V46` an election hands its key over but records no candidate. Activated straight from
    // there, `V50` seats it, so the time from the activation is the candidate's, not its key's address.
    let mut h = at(ProtocolVersion::V46, 1_000_000_000);
    switch(&mut h, true);
    seat(&mut h, 1, CANDIDATE, key());
    stake(&mut h, STAKER_A, 1);
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(key()));
    assert_eq!(h.engine.operator_candidate(), None);
    h.advance(6 * HOUR_MS);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V50 });
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
    let events = close(&mut h);
    assert_eq!(shares(&events), vec![(CANDIDATE, 200_000_000)]);
    assert_eq!(leaf(&h, key()), 0);
}

#[test]
fn staking_initialised_under_v50_counts_from_its_initialisation() {
    let floor = Fixed::from_int(HOUSE_CAPITAL) - Fixed::from_raw(1_000_000_000 * USDC);
    let mut h = Harness::v21();
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V50 });
    let start_ms = h.ts + HOUR_MS;
    let config = engine_core::StakingConfig { liquidity_floor: floor, epoch_ms: DAY_MS, start_ms, cooldown_ms: DAY_MS };
    h.operator_admin(AdminCommand::InitStaking(config));
    assert_eq!(office(&h), TimeInOffice { since: start_ms - HOUR_MS, served: Default::default() });
    switch(&mut h, true);
    stake(&mut h, STAKER_A, 1);
    // The hour before epoch 0 starts counts too, as the first close pays what built up in it.
    let events = close(&mut h);
    assert_eq!(shares(&events), vec![(operator().address, 200_000_000)]);
}

#[test]
fn a_replay_reaches_the_same_root_and_time_in_office() {
    let mut h = rotating_v50(1_000_000_000);
    stake(&mut h, STAKER_A, 1);
    seat(&mut h, 1, CANDIDATE, key());
    h.cmd(Command::Tick);
    h.advance(5 * HOUR_MS);
    h.cmd(Command::Tick);
    assert!(office(&h).served.contains_key(&CANDIDATE), "{:?}", office(&h));
    let root = h.engine.state_root();
    let mut replayed = common::batch::replayed(&h);
    assert_eq!(replayed.state_root(), root);
    assert_eq!(replayed.staking(), h.engine.staking());
}

#[test]
fn the_split_never_allocates_more_than_the_excess() {
    let config =
        Config { cases: 48, rng_seed: RngSeed::Fixed(0x5eed_0050), failure_persistence: None, ..Config::default() };
    // Excess, stakes, when in the epoch the office changes, and how long rotation is off before the close.
    let strategy = (1u128..=1_000_000_000_000u128, 1u128..=1_000_000u128, 1..DAY_MS, 1..DAY_MS);
    let result = TestRunner::new(config).run(&strategy, |(excess_units, stake_amount, handover, off)| {
        let mut h = rotating_v50(excess_units);
        let start = end_of(&h, 7);
        elect(&mut h, 1, CANDIDATE, key(), start + handover);
        h.advance_to(start);
        h.cmd(Command::Tick);
        stake(&mut h, STAKER_A, stake_amount);
        h.advance_to(start + handover);
        finalised(&mut h, start + handover);
        h.cmd(Command::Tick);
        let switch_off = (start + DAY_MS - off).max(start + handover);
        h.advance_to(switch_off);
        switch(&mut h, false);
        let events = close(&mut h);

        let fifth = excess(&events) / 5;
        let parts: BTreeMap<AccountId, u128> = shares(&events).into_iter().collect();
        let (outgoing, incoming) = (handover, switch_off - start - handover);
        let expected: BTreeMap<AccountId, u128> =
            [(operator().address, part(fifth, outgoing, DAY_MS)), (CANDIDATE, part(fifth, incoming, DAY_MS))]
                .into_iter()
                .filter(|(_, amount)| *amount > 0)
                .collect();
        let credited = leaf(&h, operator().address) + leaf(&h, CANDIDATE) + leaf(&h, STAKER_A);
        let check = |holds: bool, what: &str| {
            if holds { Ok(()) } else { Err(TestCaseError::fail(format!("{what}: {events:?}"))) }
        };
        check(parts == expected, "each part is the fifth by its time in office, rounded down")?;
        check(parts.values().sum::<u128>() <= fifth, "the parts never pass the fifth")?;
        check(credited == allocated(&events).0, "the leaves sum to what the close allocated")?;
        check(credited <= excess(&events), "nothing past the excess")?;
        // The only staker takes everything the parts leave.
        check(credited == excess(&events), "the time no one served and the rounding go to the staker")?;
        Ok(())
    });
    if let Err(error) = result {
        panic!("{error}");
    }
}
