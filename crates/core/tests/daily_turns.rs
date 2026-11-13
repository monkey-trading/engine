//! Daily operator turns from `V51`, through `Engine::apply`: the schedule's rotation sets and turn
//! assignments mirrored once under their chain references, a drawn key signing from its boundary, a
//! reassignment or promotion after its boundary from its mirror, a key without seven days' notice in a
//! mirrored set refused, `RotateOperator` refused while rotation is on, and `TurnTaken` accepted only
//! for the turn in effect; from `V53`, a member's notice kept across sets that name it unchanged
//! (`daily_turns/notice.rs`).

mod common;

use common::close_spread::{ceilings, install_expect};
use common::{BTC, Client, Harness, OPERATOR_SEED, operator};
use engine_core::command::{AdminCommand, Command};
use engine_core::election::RegistryElection;
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::turns::{DAY_MS, RegistryRotationSet, RegistryTurn, SetMember, TurnReason, boundary};
use engine_core::types::{AccountId, Millis};

/// The seeds of the three members' admin keys.
const SEEDS: [u8; 3] = [0x61, 0x62, 0x63];
const CANDIDATES: [AccountId; 3] = [AccountId([0xa1; 20]), AccountId([0xa2; 20]), AccountId([0xa3; 20])];

fn key(member: usize) -> AccountId {
    Client::new(SEEDS[member]).account
}

/// `v37`, then `V51`, with rotation on.
fn rotating() -> Harness {
    let mut h = Harness::v37();
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V51 });
    let (block, chain_ref) = (h.log.len() as u64, "0xon:0".to_owned());
    h.cmd(Command::SwitchRotation { enabled: true, election_cancelled: false, block, chain_ref });
    h
}

/// The three members, weighted 50, 30 and 20.
fn members() -> Vec<SetMember> {
    [50, 30, 20]
        .into_iter()
        .enumerate()
        .map(|(i, weight)| SetMember { candidate: CANDIDATES[i], admin_key: key(i), weight: TokenUnits(weight) })
        .collect()
}

/// Mirrors set `set` of `members`, returning its events.
fn adopt(h: &mut Harness, set: u32, members: Vec<SetMember>) -> Vec<Event> {
    let effective_at = h.ts + NOTICE_MS + 60_000;
    adopt_effective(h, set, members, effective_at)
}

/// `adopt`, taking effect on chain at `effective_at`.
fn adopt_effective(h: &mut Harness, set: u32, members: Vec<SetMember>, effective_at: Millis) -> Vec<Event> {
    let (block, chain_ref) = (h.log.len() as u64, format!("0xset:{set}:{}", h.log.len()));
    let adoption =
        RegistryRotationSet { set, epoch: set.into(), effective_at, quorum: true, members, block, chain_ref };
    h.cmd(Command::AdoptRotationSet(adoption))
}

/// Mirrors `turn` for `member` of set 1 at `position` for `reason`, returning its events.
fn assign(h: &mut Harness, turn: u64, member: usize, position: u16, reason: TurnReason) -> Vec<Event> {
    assign_from(h, 1, turn, member, position, reason)
}

/// `assign`, drawn from set `set`.
fn assign_from(h: &mut Harness, set: u32, turn: u64, member: usize, position: u16, reason: TurnReason) -> Vec<Event> {
    let (block, chain_ref) = (h.log.len() as u64, format!("0xturn:{turn}:{position}"));
    h.cmd(Command::AssignTurn(RegistryTurn {
        turn,
        set,
        position,
        reason,
        candidate: CANDIDATES[member],
        admin_key: key(member),
        block,
        chain_ref,
    }))
}

/// The first turn whose boundary is after `ts`.
fn next_turn(ts: Millis) -> u64 {
    (0..).find(|turn| boundary(*turn).expect("in range") > ts).expect("a later turn")
}

/// `rotating` with set 1 mirrored and the clock eight days on, a day before the next boundary.
fn with_set() -> (Harness, u64) {
    let mut h = rotating();
    assert!(matches!(adopt(&mut h, 1, members()).last(), Some(Event::RotationSetAdopted { set: 1, .. })));
    let turn = next_turn(h.ts + 8 * DAY_MS);
    h.advance_to(boundary(turn).unwrap() - DAY_MS);
    (h, turn)
}

/// The outcome of a reduce-only change in BTC signed by the key from `seed`.
fn signed_by(h: &mut Harness, seed: u8, reduce_only: bool) -> Option<Event> {
    h.operate_as(seed, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only }).pop()
}

fn refused(events: &[Event], reason: &str) {
    assert!(
        matches!(events.last(), Some(Event::ElectionRejected { reason: r, .. }) if r.contains(reason)),
        "{events:?}"
    );
}

fn turn_refused(events: &[Event], reason: &str) {
    assert!(matches!(events.last(), Some(Event::TurnRefused { reason: r, .. }) if r.contains(reason)), "{events:?}");
}

fn operator_key(h: &Harness) -> AccountId {
    h.engine.operator().expect("an operator").address
}

fn release(byte: u8) -> alloy_primitives::B256 {
    alloy_primitives::B256::repeat_byte(byte)
}

#[test]
fn schedule_mirrors_need_v51_and_each_credits_once() {
    let mut h = Harness::v37();
    refused(&adopt(&mut h, 1, members()), "daily turns need protocol V51");
    assert!(h.engine.daily_turns().is_default(), "nothing recorded before V51");

    let (mut h, turn) = with_set();
    let events = assign(&mut h, turn, 0, 0, TurnReason::Draw);
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
    let replay = h.log.last().expect("the turn's mirror").cmd.clone();
    refused(&h.cmd(replay), "is already mirrored");
    let adoption = h.log.iter().find(|s| matches!(s.cmd, Command::AdoptRotationSet(_))).expect("the set's mirror");
    let replay = adoption.cmd.clone();
    refused(&h.cmd(replay), "is already mirrored");
}

#[test]
fn a_set_out_of_order_or_malformed_is_refused() {
    let (mut h, _) = with_set();
    refused(&adopt(&mut h, 1, members()), "set 1 is not after the last mirrored");
    let mut zero = members();
    zero[1].weight = TokenUnits(0);
    refused(&adopt(&mut h, 2, zero), "positive weights");
    let mut twice = members();
    twice[2].candidate = twice[0].candidate;
    refused(&adopt(&mut h, 2, twice), "each candidate and key once");
    let many = |count: u8| {
        (1..=count)
            .map(|i| SetMember { candidate: AccountId([i; 20]), admin_key: AccountId([i; 20]), weight: TokenUnits(1) })
            .collect::<Vec<_>>()
    };
    refused(&adopt(&mut h, 2, many(102)), "at most 101 members, not 102");
    assert_eq!(h.engine.daily_turns().sets.len(), 1, "no refused set is recorded");
    // The registry's cap and the operator in effect's place beyond it, and a close that leaves no one.
    assert!(matches!(adopt(&mut h, 2, many(101)).last(), Some(Event::RotationSetAdopted { set: 2, .. })));
    assert!(matches!(adopt(&mut h, 3, vec![]).last(), Some(Event::RotationSetAdopted { set: 3, .. })));
    assert!(h.engine.daily_turns().sets[&3].members.is_empty());
}

#[test]
fn every_set_a_turn_yet_to_end_can_draw_from_is_kept() {
    let kept = |h: &Harness| h.engine.daily_turns().sets.keys().copied().collect::<Vec<_>>();
    let mut h = rotating();
    // Five closes a day apart, each set waiting out its own notice.
    for set in 1..=5 {
        assert!(matches!(adopt(&mut h, set, members()).last(), Some(Event::RotationSetAdopted { .. })));
        h.advance_to(h.ts + DAY_MS);
    }
    assert_eq!(kept(&h), [1, 2, 3, 4, 5]);
    // The last turn whose deadline finds set 1 in effect ends two days after set 2 takes effect, at
    // the latest; a minute before it ends, set 1 is still kept, and the turn's draw from it applies.
    let second = h.engine.daily_turns().sets[&2].effective_at;
    let turn = next_turn(second);
    assert!(boundary(turn).unwrap() - DAY_MS < second, "turn {turn} draws from set 1");
    h.advance_to(boundary(turn).unwrap() + DAY_MS - 60_000);
    assert!(matches!(adopt(&mut h, 6, members()).last(), Some(Event::RotationSetAdopted { .. })));
    assert_eq!(kept(&h), [1, 2, 3, 4, 5, 6]);
    let events = assign(&mut h, turn, 1, 0, TurnReason::Draw);
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
    // Two days after set 2 took effect no turn yet to end draws from set 1, and it is forgotten.
    h.advance_to(second + 2 * DAY_MS);
    assert!(matches!(adopt(&mut h, 7, members()).last(), Some(Event::RotationSetAdopted { .. })));
    assert_eq!(kept(&h), [2, 3, 4, 5, 6, 7]);
}

#[test]
fn a_drawn_key_mirrored_before_its_boundary_signs_from_the_boundary() {
    let (mut h, turn) = with_set();
    let start = boundary(turn).unwrap();
    let events = assign(&mut h, turn, 1, 0, TurnReason::Draw);
    assert_eq!(
        events.last(),
        Some(&Event::TurnAssigned {
            turn,
            position: 0,
            reason: TurnReason::Draw,
            candidate: CANDIDATES[1],
            admin_key: key(1),
            applies_at: start,
        })
    );
    // Before the boundary the key in effect signs and the drawn one does not.
    h.advance_to(start - 1);
    assert_eq!(signed_by(&mut h, SEEDS[1], true), Some(Event::AdminRejected { reason: "invalid signature".into() }));
    assert_eq!(signed_by(&mut h, OPERATOR_SEED, true), Some(Event::AdminApplied));
    // From the boundary the drawn key signs, and the one before it signs nothing.
    h.advance_to(start);
    assert_eq!(
        signed_by(&mut h, OPERATOR_SEED, false),
        Some(Event::AdminRejected { reason: "invalid signature".into() })
    );
    assert_eq!(operator_key(&h), key(1));
    assert_eq!(signed_by(&mut h, SEEDS[1], false), Some(Event::AdminApplied));
    assert_eq!(h.engine.daily_turns().in_effect.map(|in_effect| in_effect.turn), Some(turn));
    assert!(h.engine.daily_turns().assigned.is_empty());

    // The next day's turn hands over from its boundary in turn.
    let next = turn + 1;
    assert!(matches!(assign(&mut h, next, 2, 0, TurnReason::Draw).last(), Some(Event::TurnAssigned { .. })));
    h.advance_to(boundary(next).unwrap());
    assert!(matches!(signed_by(&mut h, SEEDS[1], true), Some(Event::AdminRejected { .. })));
    assert_eq!(signed_by(&mut h, SEEDS[2], true), Some(Event::AdminApplied));
}

#[test]
fn a_reassignment_mirrored_after_its_boundary_applies_from_its_mirror() {
    for reason in [TurnReason::Skip, TurnReason::Promotion] {
        let (mut h, turn) = with_set();
        assign(&mut h, turn, 0, 0, TurnReason::Draw);
        h.advance_to(boundary(turn).unwrap() + 60_000);
        assert_eq!(signed_by(&mut h, SEEDS[0], true), Some(Event::AdminApplied));
        // Mirrored only once finalised, a promotion applies from its mirror: until then the drawn
        // key signs.
        h.advance(60_000);
        assert_eq!(signed_by(&mut h, SEEDS[0], false), Some(Event::AdminApplied));
        let mirrored = h.ts;
        let events = assign(&mut h, turn, 2, 1, reason);
        assert!(
            matches!(events.last(), Some(Event::TurnAssigned { applies_at, position: 1, .. }) if *applies_at == mirrored),
            "{events:?}"
        );
        assert!(matches!(signed_by(&mut h, SEEDS[0], true), Some(Event::AdminRejected { .. })), "{reason:?}");
        assert_eq!(signed_by(&mut h, SEEDS[2], true), Some(Event::AdminApplied), "{reason:?}");
    }
}

#[test]
fn a_key_no_set_named_seven_days_earlier_is_refused_and_the_key_in_effect_stays() {
    let mut h = rotating();
    adopt(&mut h, 1, members());
    // A turn applying six days after its set was logged has had less than the notice.
    let turn = next_turn(h.ts + 6 * DAY_MS);
    h.advance_to(boundary(turn).unwrap() - DAY_MS);
    turn_refused(&assign(&mut h, turn, 0, 0, TurnReason::Draw), "less than seven days before the turn applies");
    h.advance_to(boundary(turn).unwrap());
    assert_eq!(signed_by(&mut h, OPERATOR_SEED, true), Some(Event::AdminApplied));
    assert_eq!(operator_key(&h), operator().address);

    // A key the set does not name, or names for another candidate, is refused however long ago.
    let (mut h, turn) = with_set();
    let (block, chain_ref) = (h.log.len() as u64, "0xturn:stranger".to_owned());
    let stranger = RegistryTurn {
        turn,
        set: 1,
        position: 0,
        reason: TurnReason::Draw,
        candidate: CANDIDATES[0],
        admin_key: key(1),
        block,
        chain_ref,
    };
    turn_refused(&h.cmd(Command::AssignTurn(stranger.clone())), "does not name candidate");
    let unknown = RegistryTurn { set: 7, chain_ref: "0xturn:unknown".into(), ..stranger };
    turn_refused(&h.cmd(Command::AssignTurn(unknown)), "set 7 is not among the sets mirrored");
    assert!(h.engine.daily_turns().assigned.is_empty());
}

#[test]
fn a_turn_already_over_is_credited_and_refused() {
    let (mut h, turn) = with_set();
    assign(&mut h, turn + 1, 0, 0, TurnReason::Draw);
    h.advance_to(boundary(turn + 1).unwrap());
    h.cmd(Command::Tick);
    turn_refused(&assign(&mut h, turn, 1, 0, TurnReason::Draw), "ended before its assignment was mirrored");
    h.advance(DAY_MS / 2);
    // A reassignment of the turn in effect applies; once that turn is over, one is refused.
    h.engine.daily_turns().in_effect.expect("a turn in effect");
    let (block, chain_ref) = (h.log.len() as u64, "0xturn:late".to_owned());
    let late = RegistryTurn {
        turn: turn + 1,
        set: 1,
        position: 1,
        reason: TurnReason::Skip,
        candidate: CANDIDATES[2],
        admin_key: key(2),
        block,
        chain_ref,
    };
    assert!(matches!(h.cmd(Command::AssignTurn(late)).last(), Some(Event::TurnAssigned { .. })));
    h.advance(DAY_MS);
    turn_refused(&assign(&mut h, turn + 1, 1, 2, TurnReason::Skip), "ended before its assignment was mirrored");
}

#[test]
fn rotate_operator_stays_refused_while_turns_run() {
    let (mut h, turn) = with_set();
    assign(&mut h, turn, 0, 0, TurnReason::Draw);
    h.advance_to(boundary(turn).unwrap());
    let events = h.operate_as(SEEDS[0], AdminCommand::RotateOperator { address: key(2) });
    assert_eq!(
        events.last(),
        Some(&Event::AdminRejected { reason: "while rotation is on, only an election moves the operator key".into() })
    );
}

#[test]
fn drawn_turns_stand_after_rotation_turns_off_until_an_installation_ends_them() {
    let (mut h, turn) = with_set();
    assign(&mut h, turn, 0, 0, TurnReason::Draw);
    assign(&mut h, turn + 1, 1, 0, TurnReason::Draw);
    h.advance_to(boundary(turn).unwrap());
    let (block, chain_ref) = (h.log.len() as u64, "0xoff".to_owned());
    h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: false, block, chain_ref });
    // The drawn turns still name the operator: no signed rotation, and the next turn's key applies.
    let events = h.operate_as(SEEDS[0], AdminCommand::RotateOperator { address: key(2) });
    assert!(matches!(events.last(), Some(Event::AdminRejected { .. })), "{events:?}");
    h.advance_to(boundary(turn + 1).unwrap());
    assert_eq!(signed_by(&mut h, SEEDS[1], true), Some(Event::AdminApplied));

    // The timelock installs an operator once the drawn turns end: the turns end with it, and a signed
    // rotation is no longer refused for them.
    h.advance_to(boundary(turn + 2).unwrap());
    let installed = Client::new(0x6a).account;
    let (block, chain_ref) = (h.log.len() as u64, "0xinstall".to_owned());
    let installation = engine_core::election::RegistryInstallation {
        candidate: AccountId([0xb9; 20]),
        admin_key: installed,
        block,
        chain_ref,
    };
    let events = h.cmd(Command::InstallOperator(installation));
    assert!(matches!(events.last(), Some(Event::OperatorInstalled { .. })), "{events:?}");
    let turns = h.engine.daily_turns();
    assert!(turns.in_effect.is_none() && turns.assigned.is_empty() && turns.taken.is_none(), "{turns:?}");
    h.advance(NOTICE_MS);
    assert_eq!(signed_by(&mut h, 0x6a, false), Some(Event::AdminApplied));
}

#[test]
fn a_turn_is_taken_before_any_catch_up_in_its_command() {
    let (mut h, turn) = with_set();
    assign(&mut h, turn, 0, 0, TurnReason::Draw);
    h.advance_to(boundary(turn).unwrap());
    h.cmd(Command::Tick);
    // A day on, a rollover is due: the turn's `TurnTaken` charges nothing, and the next command does.
    h.advance(DAY_MS - 1);
    let catch_up = |event: &Event| matches!(event, Event::BenchmarkRateMissing { .. } | Event::Financing { .. });
    let taken = h.cmd(Command::TurnTaken { turn, release: release(1) });
    assert_eq!(taken.last(), Some(&Event::TurnTaken { turn, candidate: CANDIDATES[0], release: release(1) }));
    assert!(!taken.iter().any(catch_up), "TurnTaken caught up: {taken:?}");
    let tick = h.cmd(Command::Tick);
    assert!(tick.iter().any(catch_up), "the next command catches up: {tick:?}");
}

#[test]
fn turn_taken_is_accepted_once_and_only_for_the_turn_in_effect() {
    let (mut h, turn) = with_set();
    assign(&mut h, turn, 0, 0, TurnReason::Draw);
    let early = h.cmd(Command::TurnTaken { turn, release: release(1) });
    let reason = format!("turn {turn} is not the turn in effect");
    assert_eq!(early.last(), Some(&Event::TurnTakenRejected { reason }));
    h.advance_to(boundary(turn).unwrap());
    let taken = h.cmd(Command::TurnTaken { turn, release: release(1) });
    assert_eq!(taken.last(), Some(&Event::TurnTaken { turn, candidate: CANDIDATES[0], release: release(1) }));
    let again = h.cmd(Command::TurnTaken { turn, release: release(1) });
    assert_eq!(again.last(), Some(&Event::TurnTakenRejected { reason: format!("turn {turn} is already taken") }));
    // A reassignment of the same turn is taken afresh by its own member.
    h.advance(60_000);
    assign(&mut h, turn, 1, 1, TurnReason::Promotion);
    let promoted = h.cmd(Command::TurnTaken { turn, release: release(2) });
    assert_eq!(promoted.last(), Some(&Event::TurnTaken { turn, candidate: CANDIDATES[1], release: release(2) }));

    let mut before = Harness::v37();
    let events = before.cmd(Command::TurnTaken { turn, release: release(1) });
    let reason = "taking a turn needs protocol V51".to_owned();
    assert_eq!(events.last(), Some(&Event::TurnTakenRejected { reason }));
}

#[test]
fn the_due_key_names_the_next_turns_key_from_its_boundary() {
    let (mut h, turn) = with_set();
    assign(&mut h, turn, 2, 0, TurnReason::Draw);
    let start = boundary(turn).unwrap();
    assert_eq!(h.engine.due_key(start - 1), None);
    assert_eq!(h.engine.due_key(start).map(|due| due.address), Some(key(2)));
}

#[test]
fn an_election_is_refused_once_the_schedule_adopted_a_set() {
    let (mut h, _) = with_set();
    let (block, chain_ref) = (h.log.len() as u64, "0xelect".to_owned());
    let election = RegistryElection {
        epoch: 9,
        candidate: CANDIDATES[0],
        admin_key: key(0),
        effective_at: h.ts + NOTICE_MS,
        block,
        chain_ref,
    };
    refused(&h.cmd(Command::ElectOperator(election)), "whose schedule adopts rotation sets");
}

#[path = "daily_turns/notice.rs"]
mod notice;
