//! `V51`: under daily turns the operator's share counts a member's time in office from its
//! `TurnTaken` to the end of its turn. The time between a turn's boundary and its member's
//! `TurnTaken`, and an incumbent's time past its turn, go to stakers. From `V53` the account in office
//! before any turn is in effect holds it only until the first turn applies.

use engine_core::staking::TimeInOffice;
use engine_core::turns::{DAY_MS, RegistryRotationSet, RegistryTurn, SetMember, TurnReason, boundary};

use super::*;

const HOUR_MS: Millis = 60 * 60 * 1000;
const MEMBER: AccountId = AccountId([0xd1; 20]);

/// `at` under `V51` with rotation on and a set naming `MEMBER` under `key()` mirrored now.
fn with_set(excess: u128) -> Harness {
    with_set_under(ProtocolVersion::V51, excess)
}

/// `with_set` under `version`.
fn with_set_under(version: ProtocolVersion, excess: u128) -> Harness {
    let mut h = at(version, excess);
    switch(&mut h, true);
    let members = vec![SetMember { candidate: MEMBER, admin_key: key(), weight: TokenUnits(1) }];
    let (block, chain_ref) = (h.log.len() as u64, "0xset:1".to_owned());
    let effective_at = h.ts + NOTICE_MS;
    let adoption = RegistryRotationSet { set: 1, epoch: 1, effective_at, quorum: true, members, block, chain_ref };
    assert!(matches!(h.cmd(Command::AdoptRotationSet(adoption)).last(), Some(Event::RotationSetAdopted { .. })));
    h
}

/// Mirrors `turn` for `MEMBER` of set 1.
fn assign(h: &mut Harness, turn: u64) {
    let (block, chain_ref) = (h.log.len() as u64, format!("0xturn:{turn}"));
    let reason = TurnReason::Draw;
    let assignment =
        RegistryTurn { turn, set: 1, position: 0, reason, candidate: MEMBER, admin_key: key(), block, chain_ref };
    assert!(matches!(h.cmd(Command::AssignTurn(assignment)).last(), Some(Event::TurnAssigned { .. })));
}

/// The open epoch's time in office.
fn office(h: &Harness) -> TimeInOffice {
    h.engine.staking().and_then(|staking| staking.office.clone()).expect("V50 counts time in office")
}

/// Every operator's part among `events`.
fn shares(events: &[Event]) -> Vec<(AccountId, u128)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::OperatorShareAllocated { account, amount, .. } => Some((*account, amount.0)),
            _ => None,
        })
        .collect()
}

/// The epoch, after the set's notice, whose span holds a turn's boundary, with that turn.
fn epoch_with_boundary(h: &Harness) -> (Millis, u64) {
    let config = h.engine.staking().expect("staking is initialised").config;
    let mut epoch = config.epochs_elapsed(h.ts + NOTICE_MS + DAY_MS);
    loop {
        let (start, end) = (config.epoch_end(epoch), config.epoch_end(epoch + 1));
        let turn = (0..).find(|turn| boundary(*turn).unwrap() > start).unwrap();
        if boundary(turn).unwrap() < end {
            return (start, turn);
        }
        epoch += 1;
    }
}

#[test]
fn a_member_earns_from_its_turn_taken_to_the_end_of_its_turn() {
    let mut h = with_set(1_000_000_000);
    let (start, turn) = epoch_with_boundary(&h);
    let begins = boundary(turn).unwrap();
    assign(&mut h, turn);
    // The close that opens the epoch pays no one, as nothing is staked, and starts the count.
    h.advance_to(start);
    h.cmd(Command::Tick);
    stake(&mut h, STAKER_A, 1);

    // Until the boundary the key in effect's address holds the office, as under `V50`; from it the
    // member's key signs, but the member earns only from its `TurnTaken`, an hour later.
    h.advance_to(begins);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(key()));
    h.advance_to(begins + HOUR_MS);
    let release = alloy_primitives::B256::repeat_byte(7);
    let taken = h.cmd(Command::TurnTaken { turn, release });
    assert_eq!(taken.last(), Some(&Event::TurnTaken { turn, candidate: MEMBER, release }));
    let close_at =
        h.engine.staking().unwrap().config.epoch_end(h.engine.staking().unwrap().config.epochs_elapsed(h.ts));
    let events = close(&mut h);
    let fifth = 200_000_000_u128;
    let part = |served: Millis| fifth * served as u128 / DAY_MS as u128;
    let mut expected = vec![(operator().address, part(begins - start)), (MEMBER, part(close_at - begins - HOUR_MS))];
    expected.sort();
    assert_eq!(shares(&events), expected);

    // The next epoch holds the turn's end: the member earns up to it and no one after, though its key
    // stays in effect with no later turn drawn.
    let ends = begins + DAY_MS;
    h.advance_to(ends + 2 * HOUR_MS);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(key()));
    assert_eq!(office(&h), TimeInOffice { since: close_at, served: [(MEMBER, ends - close_at)].into_iter().collect() });
}

#[test]
fn a_drawn_turn_keeps_earning_after_rotation_turns_off() {
    let mut h = with_set(1_000_000_000);
    let (start, turn) = epoch_with_boundary(&h);
    let begins = boundary(turn).unwrap();
    assign(&mut h, turn);
    h.advance_to(start);
    h.cmd(Command::Tick);
    h.advance_to(begins);
    let release = alloy_primitives::B256::repeat_byte(7);
    h.cmd(Command::TurnTaken { turn, release });
    // The switch turns off, but the drawn turn still names its member, which earns to the turn's end.
    h.advance(HOUR_MS);
    switch(&mut h, false);
    h.advance(HOUR_MS);
    h.cmd(Command::Tick);
    assert_eq!(office(&h).served.get(&MEMBER), Some(&(2 * HOUR_MS)));
}

#[test]
fn before_any_turn_the_account_in_office_holds_it_only_until_the_first_turn_applies_from_v53() {
    for version in [ProtocolVersion::V51, ProtocolVersion::V53] {
        let mut h = with_set_under(version, 1_000_000_000);
        let (start, turn) = epoch_with_boundary(&h);
        let begins = boundary(turn).unwrap();
        assign(&mut h, turn);
        h.advance_to(start);
        h.cmd(Command::Tick);
        stake(&mut h, STAKER_A, 1);
        // Nothing is sequenced from the boundary until the member's `TurnTaken`, an hour later.
        h.advance_to(begins + HOUR_MS);
        let release = alloy_primitives::B256::repeat_byte(7);
        let taken = h.cmd(Command::TurnTaken { turn, release });
        assert_eq!(taken.last(), Some(&Event::TurnTaken { turn, candidate: MEMBER, release }));
        let config = h.engine.staking().unwrap().config;
        let close_at = config.epoch_end(config.epochs_elapsed(h.ts));
        let events = close(&mut h);
        // Under `V51` the key in effect's address is credited the hour past the boundary; from `V53`
        // its office ends as the first turn applies, and the hour goes to stakers.
        let incumbent = if version == ProtocolVersion::V53 { begins - start } else { begins + HOUR_MS - start };
        let part = |served: Millis| 200_000_000_u128 * served as u128 / DAY_MS as u128;
        let mut expected = vec![(operator().address, part(incumbent)), (MEMBER, part(close_at - begins - HOUR_MS))];
        expected.sort();
        assert_eq!(shares(&events), expected, "{version:?}");
    }
}
