//! From `V83`, holding the staking mirror back moves no distribution. The host of the operator in office
//! can delay a close, since a close waits for the mirror to pass the epoch's end, but the operator's share
//! splits by time in office up to that end, however late the close runs, and the next epoch counts from
//! it; and once the mirror catches up, each held epoch closes in turn, as the mirror passes its end, at
//! the stake set mirrored then, so a holder that staked during the hold is paid for no epoch before it.

use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};

use crate::common::{Harness, operator};
use crate::support::{DAY_MS, EXCESS, HOUR_MS, close_at, closed, elect, end_of, frontier, harness, paid, stake};

const V75: ProtocolVersion = ProtocolVersion::V75;
const V83: ProtocolVersion = ProtocolVersion::V83;
const CANDIDATE: AccountId = AccountId([0xc1; 20]);
const STAKER: AccountId = AccountId([0xa1; 20]);
const LATE_STAKER: AccountId = AccountId([0xa2; 20]);
/// The stakers' part of the excess: four fifths, the operator in office taking the rest.
const STAKERS: u128 = EXCESS - EXCESS / 5;

/// One epoch whose excess is 1,000 USDC at `version`. The deployer's key is in office until ninety
/// minutes before the epoch ends, as a daily turn's boundary falls, and the elected candidate from then
/// on. The candidate's host lets the staking mirror pass the epoch's end `held` after it, while prices
/// and ticks go on. Returns what the outgoing operator and the candidate are paid of the epoch's fifth.
fn fifth_after_hold(version: ProtocolVersion, held: Millis) -> (u128, u128) {
    let mut h = harness(version);
    let start = end_of(&h, 7);
    let end = start + DAY_MS;
    let handover = end - 90 * 60 * 1000;
    elect(&mut h, CANDIDATE, 0x66, handover);
    assert_eq!(closed(&close_at(&mut h, start)), Some((EXCESS, 0)));
    stake(&mut h, STAKER, 1);
    h.advance_to(handover);
    frontier(&mut h, handover);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE), "the candidate is in office");
    let mut at = end;
    while at < end + held {
        h.advance_to(at);
        assert_eq!(closed(&h.cmd(Command::Tick)), None);
        at += HOUR_MS;
    }
    let events = close_at(&mut h, end + held);
    assert_eq!(closed(&events), Some((EXCESS, EXCESS)), "{events:?}");
    (paid(&h, operator().address), paid(&h, CANDIDATE))
}

#[test]
fn the_operator_in_office_earns_nothing_by_holding_a_close() {
    let fifth = EXCESS / 5;
    // The candidate served ninety minutes of the epoch's day.
    let prompt = (fifth * 225 / 240, fifth * 15 / 240);
    for held in [0, 14 * HOUR_MS] {
        assert_eq!(fifth_after_hold(V83, held), prompt, "held {held} ms");
    }
    // Before `V83` the time held was the candidate's.
    let (outgoing, candidate) = fifth_after_hold(V75, 14 * HOUR_MS);
    assert!(candidate > prompt.1 && outgoing < prompt.0, "{outgoing}, {candidate}");
}

#[test]
fn a_held_close_starts_the_next_count_at_the_epochs_end() {
    // The candidate, in office from ninety minutes before the end, holds the close three hours: those
    // hours are the next epoch's, which counts from the end.
    let mut h = harness(V83);
    let start = end_of(&h, 7);
    let end = start + DAY_MS;
    elect(&mut h, CANDIDATE, 0x66, end - 90 * 60 * 1000);
    assert_eq!(closed(&close_at(&mut h, start)), Some((EXCESS, 0)));
    stake(&mut h, STAKER, 1);
    h.advance_to(end - 90 * 60 * 1000);
    frontier(&mut h, end - 90 * 60 * 1000);
    h.cmd(Command::Tick);
    assert_eq!(closed(&close_at(&mut h, end + 3 * HOUR_MS)), Some((EXCESS, EXCESS)));
    assert_eq!(office(&h), (end, vec![(CANDIDATE, 3 * HOUR_MS)], 0));
}

/// The open epoch's count: when it began, each account's time in it, and how many stints wait for the
/// closes after it.
fn office(h: &Harness) -> (Millis, Vec<(AccountId, Millis)>, usize) {
    let office = h.engine.staking().and_then(|staking| staking.office.clone()).expect("V50 counts time in office");
    (office.since, office.served.into_iter().collect(), office.later.len())
}

/// A harness at `version` whose epoch 8 opens with one holder staked, the mirror passing its start. The
/// host then holds the mirror's reads back for three epochs while prices and ticks go on, and in the
/// third epoch's last hour a second holder stakes on chain. Returns it and when epoch 8 starts.
fn held_three_epochs(version: ProtocolVersion) -> (Harness, Millis) {
    let mut h = harness(version);
    let start = end_of(&h, 7);
    assert_eq!(closed(&close_at(&mut h, start)), Some((EXCESS, 0)));
    stake(&mut h, STAKER, 1);
    frontier(&mut h, start + HOUR_MS);
    let mut at = start + DAY_MS;
    while at <= start + 3 * DAY_MS {
        h.advance_to(at);
        assert_eq!(closed(&h.cmd(Command::Tick)), None, "no close runs while the mirror is held");
        at += 6 * HOUR_MS;
    }
    (h, start)
}

/// Every epoch closed among `events`, in order.
fn closes(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::DistributionEpochClosed { epoch, .. } => Some(*epoch),
            _ => None,
        })
        .collect()
}

#[test]
fn a_catch_up_closes_each_held_epoch_in_turn_at_the_stake_mirrored_for_its_end() {
    for (version, expected) in [(V75, (STAKERS / 2, STAKERS / 2)), (V83, (STAKERS, 0))] {
        let (mut h, start) = held_three_epochs(version);
        // The mirror catches up window by window: past the first held epoch's end, then the second's,
        // then the late stake in the third's last hour, then the third's end.
        let mut events = Vec::new();
        for epoch_end in [start + DAY_MS, start + 2 * DAY_MS] {
            h.advance(10 * 60 * 1000);
            events.extend(frontier(&mut h, epoch_end));
            events.extend(h.cmd(Command::Tick));
        }
        stake(&mut h, LATE_STAKER, 1);
        h.advance(10 * 60 * 1000);
        events.extend(frontier(&mut h, start + 3 * DAY_MS));
        events.extend(h.cmd(Command::Tick));
        assert_eq!((paid(&h, STAKER), paid(&h, LATE_STAKER)), expected, "{version:?}");
        if version == V83 {
            assert_eq!(closes(&events), [8, 9, 10], "{events:?}");
            assert!(!events.iter().any(|event| matches!(event, Event::DistributionEpochsSkipped { .. })));
        }
    }
}

#[test]
fn a_frontier_past_several_held_ends_closes_each_in_turn() {
    // One scan passes all three ends: the next command closes the three epochs in order, the first
    // paying the excess to the holder staked then, and each counting time in office to its own end.
    let (mut h, start) = held_three_epochs(V83);
    h.advance(10 * 60 * 1000);
    frontier(&mut h, start + 3 * DAY_MS);
    let held = h.ts - (start + 3 * DAY_MS);
    let events = h.cmd(Command::Tick);
    assert_eq!(closes(&events), [8, 9, 10], "{events:?}");
    let shares: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::OperatorShareAllocated { epoch, amount, .. } => Some((*epoch, amount.0)),
            _ => None,
        })
        .collect();
    assert_eq!(shares, [(8, EXCESS / 5)], "the later closes find no excess left");
    assert_eq!(paid(&h, STAKER), STAKERS);
    assert!(!h.engine.staking_close_due(h.ts));
    assert_eq!(office(&h), (start + 3 * DAY_MS, vec![(operator().address, held)], 0));
}

#[test]
fn a_mirror_that_keeps_up_pays_the_holder_staked_through_the_epoch() {
    for version in [V75, V83] {
        let mut h = harness(version);
        let start = end_of(&h, 7);
        assert_eq!(closed(&close_at(&mut h, start)), Some((EXCESS, 0)));
        stake(&mut h, STAKER, 1);
        // The mirror passes each epoch's end as it comes; the first close pays the excess.
        assert_eq!(closed(&close_at(&mut h, start + DAY_MS)), Some((EXCESS, EXCESS)));
        close_at(&mut h, start + 2 * DAY_MS);
        h.advance_to(start + 3 * DAY_MS - HOUR_MS);
        // Whatever was due by now (sessions, rollovers) runs first, so the stake's command stands alone.
        h.cmd(Command::Tick);
        stake(&mut h, LATE_STAKER, 1);
        close_at(&mut h, start + 3 * DAY_MS);
        assert_eq!((paid(&h, STAKER), paid(&h, LATE_STAKER)), (STAKERS, 0), "{version:?}");
    }
}

#[test]
fn the_epoch_open_across_v83s_activation_closes_as_the_rules_it_leaves_would() {
    // The candidate takes office two hours before the end, and the close is held three hours past it
    // when `V83` activates: the time served before the activation was counted under the rules it leaves,
    // to the activation, so this one close splits by it; the next epoch counts from the activation.
    let mut h = harness(ProtocolVersion::V80);
    let start = end_of(&h, 7);
    let end = start + DAY_MS;
    let handover = end - 2 * HOUR_MS;
    elect(&mut h, CANDIDATE, 0x66, handover);
    assert_eq!(closed(&close_at(&mut h, start)), Some((EXCESS, 0)));
    stake(&mut h, STAKER, 1);
    h.advance_to(handover);
    frontier(&mut h, handover);
    h.cmd(Command::Tick);
    let activated = end + 3 * HOUR_MS;
    h.advance_to(activated);
    let events = h.cmd(Command::Admin(engine_core::command::AdminCommand::ActivateProtocol { version: V83 }));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(closed(&events), None, "the close still waits for the mirror");
    frontier(&mut h, activated);
    let events = h.cmd(Command::Tick);
    assert_eq!(closed(&events), Some((EXCESS, EXCESS)), "{events:?}");
    let (fifth, counted, served) = (EXCESS / 5, (activated - start) as u128, (activated - handover) as u128);
    assert_eq!(
        (paid(&h, operator().address), paid(&h, CANDIDATE)),
        (fifth * (counted - served) / counted, fifth * served / counted)
    );
    assert_eq!(office(&h), (activated, vec![], 0));
}

#[test]
fn the_epoch_open_across_v83s_activation_counts_its_unserved_time_to_the_activation() {
    // Rotation is off for two hours of the epoch, which no one serves, and the close is held three hours
    // past the end when `V83` activates: the close splits the operator's share over the whole count the
    // rules it leaves ran, to the activation, so the time no one served still goes to stakers.
    let mut h = harness(ProtocolVersion::V80);
    let start = end_of(&h, 7);
    let end = start + DAY_MS;
    assert_eq!(closed(&close_at(&mut h, start)), Some((EXCESS, 0)));
    stake(&mut h, STAKER, 1);
    for (at, enabled) in [(start + 2 * HOUR_MS, false), (start + 4 * HOUR_MS, true)] {
        h.advance_to(at);
        let (block, chain_ref) = (h.log.len() as u64, format!("0xswitch:{}", h.log.len()));
        h.cmd(Command::SwitchRotation { enabled, election_cancelled: false, block, chain_ref });
    }
    let activated = end + 3 * HOUR_MS;
    h.advance_to(activated);
    let events = h.cmd(Command::Admin(engine_core::command::AdminCommand::ActivateProtocol { version: V83 }));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(closed(&events), None, "the close still waits for the mirror");
    h.advance_to(activated + HOUR_MS);
    frontier(&mut h, activated + HOUR_MS);
    let events = h.cmd(Command::Tick);
    assert!(closed(&events).is_some(), "{events:?}");
    let (fifth, counted) = (EXCESS / 5, (activated - start) as u128);
    let served = counted - (2 * HOUR_MS) as u128;
    assert_eq!(paid(&h, operator().address), fifth * served / counted);
    // The next epoch counts from the activation, the hour after it served.
    assert_eq!(office(&h), (activated, vec![(operator().address, HOUR_MS)], 0));
}

#[test]
fn an_activation_more_than_an_epoch_past_a_held_end_counts_the_epoch_it_falls_in_from_it() {
    // The close of epoch 8 is held past the ends of epochs 8 and 9 when `V83` activates three hours into
    // epoch 10: epoch 8's close counts to the activation, all of it served, and epoch 10 counts from the
    // activation, not from epoch 9's end.
    let mut h = harness(ProtocolVersion::V80);
    let start = end_of(&h, 7);
    assert_eq!(closed(&close_at(&mut h, start)), Some((EXCESS, 0)));
    stake(&mut h, STAKER, 1);
    let activated = start + 2 * DAY_MS + 3 * HOUR_MS;
    h.advance_to(activated);
    let events = h.cmd(Command::Admin(engine_core::command::AdminCommand::ActivateProtocol { version: V83 }));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    h.advance_to(activated + HOUR_MS);
    frontier(&mut h, activated + HOUR_MS);
    let events = h.cmd(Command::Tick);
    assert_eq!(closes(&events), vec![8, 9], "{events:?}");
    assert_eq!(paid(&h, operator().address), EXCESS / 5);
    assert_eq!(office(&h), (activated, vec![(operator().address, HOUR_MS)], 0));
}
