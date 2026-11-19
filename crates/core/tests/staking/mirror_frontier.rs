//! From `V75`, an epoch closes only once the staking mirror's finalised frontier has passed its end, as
//! an election waits for its effective time: the latest epoch both the sequenced time and the frontier
//! have passed closes, within a day of its end. A host that holds the mirror's reads back can delay a
//! close, but no close pays a stake set the mirror has not brought to the epoch's end: an epoch the
//! mirror passes only a day after its end is skipped, and its excess stays with the house for the close
//! the mirror lets through. `V74` closed on whatever stake the mirror had delivered.

use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};

use crate::common::{Harness, operator};
use crate::support::{
    DAY_MS, EXCESS, HOUR_MS, close_at, closed, elect, end_of, frontier, harness, harness_with, paid, progress, stake,
    stake_in,
};

/// The host's own staking account, which leaves the staking contract during the epoch.
const LEAVER: AccountId = AccountId([0xa1; 20]);
/// A holder that stakes on chain during the epoch.
const JOINER: AccountId = AccountId([0xa2; 20]);
const V74: ProtocolVersion = ProtocolVersion::V74;
const V75: ProtocolVersion = ProtocolVersion::V75;
/// The stakers' part of the excess: four fifths, the operator in office taking the rest.
const STAKERS: u128 = EXCESS / 5 * 4;

/// A harness at `version` whose epoch 7 has closed with the mirror at its end, so epoch 8 is open
/// with nothing staked. Returns it and when epoch 8 ends.
fn open_epoch(version: ProtocolVersion) -> (Harness, Millis) {
    let mut h = harness(version);
    let start = end_of(&h, 7);
    assert_eq!(closed(&close_at(&mut h, start)), Some((EXCESS, 0)));
    (h, start + DAY_MS)
}

/// The staking mirror delivers the leaver's `UnstakeCompleted` in block `first + 10` and the joiner's
/// `Staked` in block `first + 11`, both finalised at `chain_time`, then completes a scan through them.
fn deliver(h: &mut Harness, first: u64, chain_time: Millis) {
    let chain_ref = "0xbb:0".into();
    let left = Command::Unstake { account: LEAVER, amount: TokenUnits(1), block: first + 10, chain_ref };
    assert!(matches!(h.cmd(left)[..], [Event::Unstaked { .. }]));
    stake_in(h, JOINER, 1, first + 11);
    progress(h, first + 11, chain_time);
}

#[test]
fn a_close_pays_the_stake_the_mirror_finalised_by_the_epochs_end() {
    for (version, expected) in [(V75, (0, STAKERS)), (V74, (STAKERS, 0))] {
        let (mut h, end) = open_epoch(version);
        let start = end - DAY_MS;
        // Block `first`, finalised at the epoch's start: the leaver's stake.
        let first = h.log.len() as u64 + 1_000;
        stake_in(&mut h, LEAVER, 1, first);
        progress(&mut h, first, start);
        // The leaver's unstake and the joiner's stake finalise eighteen hours in, but the host holds the
        // mirror's reads back past the epoch's end while prices and ticks go on.
        h.advance_to(end);
        let events = h.cmd(Command::Tick);
        assert_eq!(closed(&events).is_some(), version == V74, "{version:?}: {events:?}");
        h.advance(HOUR_MS);
        deliver(&mut h, first, start + 18 * HOUR_MS);
        // The mirror is let through past the end, and the next command closes the epoch.
        progress(&mut h, first + 20, end + HOUR_MS);
        h.cmd(Command::Tick);
        assert_eq!((paid(&h, LEAVER), paid(&h, JOINER)), expected, "{version:?}");
    }
}

#[test]
fn a_close_waits_for_the_mirror_to_pass_the_epochs_end() {
    let (mut h, end) = open_epoch(V75);
    stake(&mut h, LEAVER, 1);
    let open = h.engine.staking().expect("staking").epoch;
    h.advance_to(end);
    assert!(!h.engine.staking_close_due(end), "the close waits for the mirror");
    assert_eq!(closed(&h.cmd(Command::Tick)), None);
    // A frontier a millisecond short of the end holds the close; the command that records the frontier
    // at the end closes nothing itself, as the close runs before it acts.
    h.advance(10 * 60 * 1000);
    assert_eq!(closed(&frontier(&mut h, end - 1)), None);
    assert_eq!(closed(&h.cmd(Command::Tick)), None);
    assert_eq!(closed(&frontier(&mut h, end)), None);
    assert!(h.engine.staking_close_due(h.ts), "the mirror has passed the end");
    assert_eq!(h.engine.staking().expect("staking").epoch, open);
    assert_eq!(paid(&h, LEAVER), 0);
    // The next command closes the epoch the mirror has passed, though its end was ten minutes ago.
    let events = h.cmd(Command::Tick);
    assert!(events.iter().any(|event| matches!(event, Event::DistributionEpochClosed { epoch, .. } if *epoch == open)));
    assert_eq!(closed(&events), Some((EXCESS, EXCESS)));
    assert_eq!(paid(&h, LEAVER), STAKERS);
}

#[test]
fn no_epoch_closes_before_the_mirror_records_a_frontier() {
    let mut h = harness(V75);
    stake(&mut h, LEAVER, 1);
    for epoch in 0..3 {
        h.advance_to(end_of(&h, epoch));
        assert_eq!(closed(&h.cmd(Command::Tick)), None);
    }
    assert_eq!(h.engine.staking().expect("staking").epoch, 0);
    let end = end_of(&h, 2);
    frontier(&mut h, end);
    let events = h.cmd(Command::Tick);
    assert!(events.contains(&Event::DistributionEpochsSkipped { first: 0, last: 1 }), "{events:?}");
    assert_eq!(closed(&events), Some((EXCESS, EXCESS)));
    assert_eq!(paid(&h, LEAVER), STAKERS);
}

#[test]
fn epochs_shorter_than_the_mirrors_lag_close_in_turn() {
    // Ten-minute epochs, and a mirror that stays fifteen minutes behind the sequenced time.
    let (epoch_ms, lag) = (10 * 60 * 1000, 15 * 60 * 1000);
    let mut h = harness_with(V75, epoch_ms);
    let start = end_of(&h, 7);
    assert_eq!(closed(&close_at(&mut h, start)), Some((EXCESS, 0)));
    stake(&mut h, LEAVER, 1);
    let mut closes = Vec::new();
    for step in 1..=12 {
        h.advance_to(start + step * 5 * 60 * 1000);
        let reached = h.ts - lag;
        let mut events = if reached > start { frontier(&mut h, reached) } else { Vec::new() };
        events.extend(h.cmd(Command::Tick));
        assert!(!events.iter().any(|event| matches!(event, Event::DistributionEpochsSkipped { .. })), "{events:?}");
        closes.extend(events.iter().filter_map(|event| match event {
            Event::DistributionEpochClosed { epoch, .. } => Some(*epoch),
            _ => None,
        }));
    }
    // Each epoch whose end the mirror passed closed once, in order, though the sequenced time had
    // already ended the next, and the first paid the excess.
    assert_eq!(closes, [8, 9, 10, 11]);
    assert_eq!(paid(&h, LEAVER), STAKERS);
}

#[test]
fn a_frontier_ahead_of_the_sequenced_time_closes_no_unended_epoch() {
    let (mut h, end) = open_epoch(V75);
    stake(&mut h, LEAVER, 1);
    // A frontier two epochs ahead, recorded before the open epoch ends, closes nothing.
    h.advance_to(end - HOUR_MS);
    assert_eq!(closed(&frontier(&mut h, end + 2 * DAY_MS)), None);
    assert_eq!(closed(&h.cmd(Command::Tick)), None);
    // At the end only the epoch the sequenced time has ended closes, and the next stays open.
    h.advance_to(end);
    let events = h.cmd(Command::Tick);
    let open = h.engine.staking().expect("staking").epoch;
    assert!(
        events.iter().any(|event| matches!(event, Event::DistributionEpochClosed { epoch, .. } if *epoch + 1 == open))
    );
    let config = h.engine.staking().expect("staking").config;
    assert_eq!(open, config.epochs_elapsed(h.ts));
}

#[test]
fn an_epoch_the_mirror_passes_a_day_after_its_end_is_skipped() {
    // A mirror that passes the end a millisecond inside the day closes the epoch.
    let (mut h, end) = open_epoch(V75);
    stake(&mut h, LEAVER, 1);
    h.advance_to(end + DAY_MS - 1);
    frontier(&mut h, end);
    let events = h.cmd(Command::Tick);
    assert_eq!(closed(&events), Some((EXCESS, EXCESS)), "{events:?}");

    let (mut h, end) = open_epoch(V75);
    stake(&mut h, LEAVER, 1);
    let open = h.engine.staking().expect("staking").epoch;
    // The mirror stalls before the end, and passes it only a day after it, while the next epoch, which
    // ends then, waits for the mirror to pass its own end.
    let next_end = end + DAY_MS;
    h.advance_to(next_end);
    assert_eq!(closed(&h.cmd(Command::Tick)), None);
    // The joiner staked on chain an hour after `open` ended, before the next epoch's end.
    stake(&mut h, JOINER, 1);
    assert_eq!(closed(&frontier(&mut h, end + HOUR_MS)), None);
    assert_eq!(closed(&h.cmd(Command::Tick)), None);
    h.advance(HOUR_MS);
    frontier(&mut h, next_end);
    let events = h.cmd(Command::Tick);
    assert!(events.contains(&Event::DistributionEpochsSkipped { first: open, last: open }), "{events:?}");
    assert!(
        events.iter().any(|event| matches!(event, Event::DistributionEpochClosed { epoch, .. } if *epoch == open + 1))
    );
    // The excess the house kept pays the stake set the mirror brought to the end of the epoch that closed.
    assert_eq!((paid(&h, LEAVER), paid(&h, JOINER)), (STAKERS / 2, STAKERS / 2));
}

#[test]
fn a_held_close_splits_the_operators_share_by_time_in_office_to_the_close() {
    const CANDIDATE: AccountId = AccountId([0xc1; 20]);
    let mut h = harness(V75);
    let start = end_of(&h, 7);
    let end = start + DAY_MS;
    let handover = end - 2 * HOUR_MS;
    elect(&mut h, CANDIDATE, 0x66, handover);
    assert_eq!(closed(&close_at(&mut h, start)), Some((EXCESS, 0)));
    stake(&mut h, LEAVER, 1);
    // The mirror passes the election's effective time, and the candidate takes office then, two hours
    // before the epoch ends.
    h.advance_to(handover);
    frontier(&mut h, handover);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
    // The mirror passes the epoch's end three hours late, and the time held is the candidate's.
    let close = end + 3 * HOUR_MS;
    h.advance_to(close);
    assert_eq!(closed(&frontier(&mut h, close)), None);
    let events = h.cmd(Command::Tick);
    assert_eq!(closed(&events), Some((EXCESS, EXCESS)), "{events:?}");
    let share = |account: AccountId| {
        events.iter().find_map(|event| match event {
            Event::OperatorShareAllocated { account: paid, amount, .. } if *paid == account => Some(amount.0),
            _ => None,
        })
    };
    let (fifth, counted, served) = (EXCESS / 5, (close - start) as u128, (close - handover) as u128);
    assert_eq!(share(CANDIDATE), Some(fifth * served / counted));
    assert_eq!(share(operator().address), Some(fifth * (counted - served) / counted));
    // The next epoch counts from the close.
    let office = h.engine.staking().and_then(|staking| staking.office.clone()).expect("V50 counts time in office");
    assert_eq!(office.since, close);
}

#[test]
fn the_activation_holds_a_due_epoch_for_the_mirror() {
    let (mut h, end) = open_epoch(V74);
    stake(&mut h, LEAVER, 1);
    h.advance_to(end);
    let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: V75 }));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    // The epoch was due at the activation, which held it for the mirror under `V75`'s rule.
    assert_eq!(closed(&events), None);
    frontier(&mut h, end);
    assert_eq!(closed(&h.cmd(Command::Tick)), Some((EXCESS, EXCESS)));
    assert_eq!(paid(&h, LEAVER), STAKERS);
}
