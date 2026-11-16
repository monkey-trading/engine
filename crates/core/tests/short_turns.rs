//! Five-minute operator turns in a `short-turns` development build, through `Engine::apply`: a
//! boundary every five minutes, 22:30 UTC among them; a drawn key signing from its boundary and the
//! next turn's from five minutes on; a key the log named less than a turn before it applies refused;
//! and a turn mirrored after its five minutes refused as ended. Only a `short-turns` build compiles it:
//! `cargo test -p engine-core --features short-turns --test short_turns`.
#![cfg(feature = "short-turns")]

mod common;

use common::close_spread::{ceilings, install_expect};
use common::{BTC, Client, Harness, OPERATOR_SEED};
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::turns::{
    BOUNDARY_OFFSET_MS, DRAW_LEAD_MS, RegistryRotationSet, RegistryTurn, SET_NOTICE_MS, SetMember, TURN_MS, TurnReason,
    boundary,
};
use engine_core::types::{AccountId, Millis};

/// Five minutes.
const FIVE_MINUTES: Millis = 5 * 60 * 1000;
/// 22:30 UTC on 2026-11-06.
const HALF_PAST_TEN: Millis = 1_794_004_200_000;
/// The seeds of the two members' admin keys.
const SEEDS: [u8; 2] = [0x61, 0x62];
const CANDIDATES: [AccountId; 2] = [AccountId([0xa1; 20]), AccountId([0xa2; 20])];

fn key(member: usize) -> AccountId {
    Client::new(SEEDS[member]).account
}

/// `v37`, then `V51` with rotation on, and a minute into a turn the two members' set mirrored.
fn with_set() -> Harness {
    let mut h = Harness::v37();
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V51 });
    let (block, chain_ref) = (h.log.len() as u64, "0xon:0".to_owned());
    h.cmd(Command::SwitchRotation { enabled: true, election_cancelled: false, block, chain_ref });
    let members =
        (0..2).map(|i| SetMember { candidate: CANDIDATES[i], admin_key: key(i), weight: TokenUnits(1) }).collect();
    h.advance_to(boundary(next_turn(h.ts)).unwrap() + 60_000);
    let (block, chain_ref) = (h.log.len() as u64, "0xset:1".to_owned());
    let effective_at = h.ts + 60_000;
    let adoption = RegistryRotationSet { set: 1, epoch: 1, effective_at, quorum: true, members, block, chain_ref };
    assert!(matches!(h.cmd(Command::AdoptRotationSet(adoption)).last(), Some(Event::RotationSetAdopted { .. })));
    h
}

/// Mirrors `turn` for `member` at `position` for `reason`, returning its events.
fn assign(h: &mut Harness, turn: u64, member: usize, position: u16, reason: TurnReason) -> Vec<Event> {
    let (block, chain_ref) = (h.log.len() as u64, format!("0xturn:{turn}:{position}"));
    let assignment = RegistryTurn {
        turn,
        set: 1,
        position,
        reason,
        candidate: CANDIDATES[member],
        admin_key: key(member),
        block,
        chain_ref,
    };
    h.cmd(Command::AssignTurn(assignment))
}

/// The first turn whose boundary is after `ts`.
fn next_turn(ts: Millis) -> u64 {
    u64::try_from((ts - BOUNDARY_OFFSET_MS) / TURN_MS + 1).expect("after the epoch")
}

/// The outcome of a reduce-only change in BTC signed by the key from `seed`.
fn signed_by(h: &mut Harness, seed: u8, reduce_only: bool) -> Option<Event> {
    h.operate_as(seed, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only }).pop()
}

#[test]
fn a_turn_starts_every_five_minutes_and_is_drawn_a_turn_ahead_with_a_turns_notice() {
    assert_eq!(
        (TURN_MS, BOUNDARY_OFFSET_MS, DRAW_LEAD_MS, SET_NOTICE_MS),
        (FIVE_MINUTES, 0, FIVE_MINUTES, FIVE_MINUTES)
    );
    let turn = u64::try_from(HALF_PAST_TEN / FIVE_MINUTES).unwrap();
    assert_eq!(boundary(turn), Some(HALF_PAST_TEN), "22:30 UTC is a boundary");
    assert_eq!(boundary(turn + 1), Some(HALF_PAST_TEN + FIVE_MINUTES));
    assert_eq!(next_turn(HALF_PAST_TEN - 1), turn);
    assert_eq!(next_turn(HALF_PAST_TEN), turn + 1);
}

#[test]
fn each_five_minute_turn_hands_the_key_over_at_its_boundary() {
    let mut h = with_set();
    // The first turn a turn's notice admits, mirrored at its draw deadline.
    let turn = next_turn(h.ts + SET_NOTICE_MS);
    let start = boundary(turn).unwrap();
    h.advance_to(start - DRAW_LEAD_MS);
    let events = assign(&mut h, turn, 0, 0, TurnReason::Draw);
    assert!(
        matches!(events.last(), Some(Event::TurnAssigned { applies_at, .. }) if *applies_at == start),
        "{events:?}"
    );
    h.advance_to(start - 1);
    assert_eq!(signed_by(&mut h, OPERATOR_SEED, true), Some(Event::AdminApplied));
    h.advance_to(start);
    assert_eq!(
        signed_by(&mut h, OPERATOR_SEED, false),
        Some(Event::AdminRejected { reason: "invalid signature".into() })
    );
    assert_eq!(signed_by(&mut h, SEEDS[0], false), Some(Event::AdminApplied));

    // The next turn, drawn at this one's boundary, hands over five minutes on.
    let events = assign(&mut h, turn + 1, 1, 0, TurnReason::Draw);
    let next = start + FIVE_MINUTES;
    assert!(matches!(events.last(), Some(Event::TurnAssigned { applies_at, .. }) if *applies_at == next), "{events:?}");
    h.advance_to(next - 1);
    assert_eq!(signed_by(&mut h, SEEDS[0], true), Some(Event::AdminApplied));
    h.advance_to(next);
    assert!(matches!(signed_by(&mut h, SEEDS[0], true), Some(Event::AdminRejected { .. })));
    assert_eq!(signed_by(&mut h, SEEDS[1], true), Some(Event::AdminApplied));
    assert_eq!(h.engine.daily_turns().in_effect.map(|in_effect| in_effect.turn), Some(turn + 1));
}

#[test]
fn a_key_named_less_than_a_turn_before_it_applies_is_refused() {
    let mut h = with_set();
    let logged = h.ts;
    // A turn applying under five minutes after the set was logged has had less than a turn's notice.
    let early = next_turn(logged);
    assert!(boundary(early).unwrap() < logged + FIVE_MINUTES);
    let events = assign(&mut h, early, 0, 0, TurnReason::Draw);
    assert!(
        matches!(events.last(), Some(Event::TurnRefused { reason, .. }) if reason.contains("less than one turn before")),
        "{events:?}"
    );
    // The turn after it has had a turn's notice by its boundary.
    let events = assign(&mut h, early + 1, 0, 0, TurnReason::Draw);
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
}

#[test]
fn a_turn_ends_five_minutes_after_its_boundary() {
    let mut h = with_set();
    let turn = next_turn(h.ts + SET_NOTICE_MS);
    let start = boundary(turn).unwrap();
    // Mirrored late in its five minutes, a turn applies from its mirror; once they have passed, it is over.
    h.advance_to(start + FIVE_MINUTES - 1);
    let events = assign(&mut h, turn, 0, 0, TurnReason::Promotion);
    assert!(matches!(events.last(), Some(Event::TurnAssigned { applies_at, .. }) if *applies_at == h.ts), "{events:?}");
    h.advance_to(start + 2 * FIVE_MINUTES);
    let events = assign(&mut h, turn + 1, 1, 0, TurnReason::Draw);
    assert!(
        matches!(events.last(), Some(Event::TurnRefused { reason, .. }) if reason.contains("ended before its assignment")),
        "{events:?}"
    );
}
