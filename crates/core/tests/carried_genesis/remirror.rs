//! From `V93` a carried genesis carries no stake: the new log mirrors the staking contract's events again up to the
//! source's completed frontier, with no registry event and no epoch close before that frontier holds the source's
//! total staked, which then becomes the carried frontier `V92` refuses every mirror at or below.

use alloy_primitives::B256;
use engine_core::carry::{CarryPart, StateCarried};
use engine_core::command::Command;
use engine_core::emissions::TokenUnits;
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::staking::Remirror;
use engine_core::types::Millis;

use crate::common::Harness;
use crate::fixture::{DAY_MS, STAKERS, UNSTAKED, carry, destination_at, part, refusal, source, stake, switch_rotation};

/// The frontier block a source whose last scan is recorded ahead of its time completes through.
const AHEAD: u64 = 8;

/// The source fixture, with its last completed scan at block `AHEAD`, `days` ahead of its time, so a log carrying
/// it can reach an epoch's end before it reaches that frontier.
fn source_ahead(days: Millis) -> Harness {
    let mut alpha = source();
    let time = alpha.ts + days * DAY_MS;
    let progress = Command::StakingProgress {
        block: AHEAD,
        block_hash: B256::repeat_byte(AHEAD as u8),
        block_time: Some(time),
        named_turn: None,
    };
    assert!(!alpha.cmd(progress).iter().any(|event| matches!(event, Event::AdminRejected { .. })));
    alpha.engine.state_root();
    alpha
}

/// A `V93` log whose genesis carried `alpha`, with its stakes still to mirror again.
fn carried(alpha: &Harness) -> Harness {
    let (parts, _) = alpha.engine.carry_parts(ProtocolVersion::V93).expect("the source carries");
    let mut h = destination_at(alpha, ProtocolVersion::V93);
    carry(&mut h, &parts);
    h
}

/// Mirrors the source fixture's staking contract events again, as the node reads them from the chain; the first,
/// as the first command past the carry, also closes the carried emission period.
fn mirror_source_events(h: &mut Harness) {
    for (block, (account, amount)) in (1..).zip(STAKERS) {
        assert!(matches!(stake(h, account, amount, block).last(), Some(Event::Staked { .. })));
    }
    let unstake = Command::Unstake {
        account: STAKERS[0].0,
        amount: TokenUnits(UNSTAKED),
        block: 4,
        chain_ref: "0xunstake:4".into(),
    };
    assert!(matches!(h.cmd(unstake).last(), Some(Event::Unstaked { .. })));
}

/// The events of a completed staking frontier at `block`.
fn frontier(h: &mut Harness, block: u64, block_time: Millis, named_turn: Option<Option<Millis>>) -> Vec<Event> {
    let block_hash = B256::repeat_byte(block as u8);
    h.cmd(Command::StakingProgress { block, block_hash, block_time: Some(block_time), named_turn })
}

/// Whether `events` close a distribution epoch.
fn closes(events: &[Event]) -> bool {
    events.iter().any(|event| matches!(event, Event::DistributionEpochClosed { .. }))
}

#[test]
fn a_v93_genesis_carries_no_stake_and_waits_for_them_mirrored_again() {
    let alpha = source();
    let (parts, totals) = alpha.engine.carry_parts(ProtocolVersion::V93).expect("the source carries");
    assert!(!parts.iter().any(|part| matches!(part, CarryPart::Stakes { .. })), "no part carries a stake");
    let staked: u128 = STAKERS.iter().map(|(_, amount)| amount).sum::<u128>() - UNSTAKED;
    assert_eq!((totals.accounts, totals.staked), (2, TokenUnits(staked)), "the entitled traders, and the total");

    let mut h = destination_at(&alpha, ProtocolVersion::V93);
    let events = carry(&mut h, &parts);
    assert_eq!(events, [Event::StateCarried(totals), Event::AdminApplied]);
    let theirs = alpha.engine.staking().expect("the source staked");
    let through = theirs.completed_through.expect("the source completed its scan");
    let ours = h.engine.staking().expect("the genesis stakes");
    assert_eq!(ours.remirror, Some(Remirror { through, staked_total: TokenUnits(staked) }));
    assert_eq!((ours.mirrored_through, ours.completed_through, ours.staked_total), (None, None, Some(TokenUnits(0))));
    assert_eq!(ours.carried_through, None, "the source's frontier is carried once the stakes are mirrored again");
    assert!(h.engine.staked_balances().is_empty() && h.engine.remirroring());
    assert_eq!(h.engine.entitlements(), alpha.engine.entitlements());

    // The source's events again, then its frontier: the stakes are the source's, and the re-mirror ends.
    mirror_source_events(&mut h);
    assert_eq!(h.engine.staked_balances(), alpha.engine.staked_balances());
    let time = through.time.expect("a V37 frontier has a time");
    assert!(!frontier(&mut h, through.block, time, None).iter().any(|e| matches!(e, Event::AdminRejected { .. })));
    let ours = h.engine.staking().expect("the genesis stakes");
    assert_eq!((ours.remirror, ours.completed_through), (None, Some(through)));
    assert_eq!(ours.carried_through, Some(through.block));
    assert!(!h.engine.remirroring());

    // From here a stake under a reference the re-mirror did not credit, at or below that frontier, is refused.
    let (account, amount) = STAKERS[1];
    let late = stake(&mut h, account, amount, through.block);
    let refused = Reject::CarriedFrontier { through: through.block };
    assert!(matches!(&late[..], [Event::StakingRejected { reason, .. }] if *reason == refused), "{late:?}");
    assert_eq!(h.engine.staked_balances(), alpha.engine.staked_balances());
}

#[test]
fn a_frontier_reaches_the_source_s_only_with_its_hash_and_its_total_staked() {
    let alpha = source();
    let through = alpha.engine.staking().and_then(|staking| staking.completed_through).expect("a completed scan");
    let time = through.time.expect("a V37 frontier has a time");
    let mut h = carried(&alpha);

    // One staker short of the source: the frontier at its block is refused, and one before it applies.
    let [(account, amount), ..] = STAKERS;
    stake(&mut h, account, amount, 1);
    let short = frontier(&mut h, through.block, time, None);
    assert!(refusal(&short).contains("not the source's total"), "{short:?}");
    let behind = frontier(&mut h, 5, time, None);
    assert!(!behind.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{behind:?}");
    assert!(h.engine.remirroring());

    // A frontier past the source's block, or naming a turn, is refused, and one at its block with another hash.
    assert!(refusal(&frontier(&mut h, through.block + 1, time, None)).contains("through block"));
    assert!(refusal(&frontier(&mut h, 6, time, Some(None))).contains("names a turn"));
    let elsewhere = Command::StakingProgress {
        block: through.block,
        block_hash: B256::repeat_byte(0xee),
        block_time: Some(time),
        named_turn: None,
    };
    assert!(refusal(&h.cmd(elsewhere)).contains("not the carried frontier's"));
}

#[test]
fn no_registry_event_mirrors_and_no_epoch_closes_until_the_stakes_are_mirrored_again() {
    let alpha = source_ahead(10);
    let through = alpha.engine.staking().and_then(|staking| staking.completed_through).expect("a completed scan");
    let mut h = carried(&alpha);
    mirror_source_events(&mut h);

    // The source's registry event in the blocks mirrored again is refused.
    let switch =
        Command::SwitchRotation { enabled: false, election_cancelled: false, block: 6, chain_ref: "0xr".into() };
    let events = h.cmd(switch);
    assert!(
        matches!(events.last(), Some(Event::ElectionRejected { reason, .. }) if reason.contains("mirrored again")),
        "{events:?}"
    );

    // Past the first epoch's end, with a frontier there too, nothing closes while the frontier is short of the
    // source's.
    let config = h.engine.staking().expect("the genesis stakes").config;
    let end = config.epoch_end(0);
    h.advance_to(end + 1);
    assert!(!frontier(&mut h, 6, end + 1, None).iter().any(|e| matches!(e, Event::AdminRejected { .. })));
    assert!(!closes(&h.cmd(Command::Tick)), "a close waits for the stakes mirrored again");
    assert!(!h.engine.staking_close_due(h.ts));

    // At the source's frontier the next command closes the epoch, and a registry event mirrors past it.
    let time = through.time.expect("a V37 frontier has a time");
    assert!(!frontier(&mut h, AHEAD, time, None).iter().any(|e| matches!(e, Event::AdminRejected { .. })));
    assert!(closes(&h.cmd(Command::Tick)), "the epoch closes once the stakes are mirrored again");
    let switch =
        Command::SwitchRotation { enabled: false, election_cancelled: false, block: AHEAD, chain_ref: "0xs".into() };
    let events = h.cmd(switch);
    assert!(
        matches!(events.last(), Some(Event::ElectionRejected { reason, .. }) if reason.contains("carried staking")),
        "{events:?}"
    );
    switch_rotation(&mut h, true, AHEAD + 1);
}

#[test]
fn a_v93_genesis_refuses_a_stake_part_and_a_source_still_mirroring_again() {
    let alpha = source();
    let (with_stakes, _) = alpha.engine.carry_parts(ProtocolVersion::V90).expect("the source carries");
    let mut h = destination_at(&alpha, ProtocolVersion::V93);
    let stakes = with_stakes.iter().position(|part| matches!(part, CarryPart::Stakes { .. })).expect("a stake part");
    carry(&mut h, &with_stakes[..stakes]);
    let events = part(&mut h, with_stakes[stakes].clone());
    assert!(refusal(&events).contains("carries no stakes"), "{events:?}");

    // A log part way through mirroring its own carried stakes again cannot be carried on.
    let mut mid = carried(&alpha);
    mirror_source_events(&mut mid);
    let through = alpha.engine.staking().and_then(|staking| staking.completed_through).expect("a completed scan");
    frontier(&mut mid, 5, through.time.expect("a V37 frontier has a time"), None);
    mid.engine.state_root();
    let error = mid.engine.carry_parts(ProtocolVersion::V93).expect_err("a source still mirroring again");
    assert!(error.contains("still mirroring"), "{error}");
}

#[test]
fn a_source_with_nothing_staked_hands_on_its_frontier() {
    let mut alpha = source();
    for (block, (account, amount)) in (10..).zip(STAKERS) {
        let left = if account == STAKERS[0].0 { amount - UNSTAKED } else { amount };
        let unstake =
            Command::Unstake { account, amount: TokenUnits(left), block, chain_ref: format!("0xout:{block}") };
        assert!(matches!(alpha.cmd(unstake)[..], [Event::Unstaked { .. }]));
    }
    let done =
        Command::StakingProgress { block: 20, block_hash: B256::repeat_byte(20), block_time: None, named_turn: None };
    assert!(!alpha.cmd(done).iter().any(|event| matches!(event, Event::AdminRejected { .. })));
    alpha.engine.state_root();
    let (parts, totals) = alpha.engine.carry_parts(ProtocolVersion::V93).expect("the source carries");
    assert_eq!(totals.staked, TokenUnits(0));
    let mut h = destination_at(&alpha, ProtocolVersion::V93);
    assert!(matches!(carry(&mut h, &parts).first(), Some(Event::StateCarried(StateCarried { .. }))));
    let (ours, theirs) = (h.engine.staking().expect("staked"), alpha.engine.staking().expect("staked"));
    assert_eq!((ours.remirror, ours.completed_through), (None, theirs.completed_through));
    assert_eq!(ours.carried_through, theirs.completed_through.map(|through| through.block));
    assert!(!h.engine.remirroring());
}
