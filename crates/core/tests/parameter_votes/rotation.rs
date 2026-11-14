//! When votes govern, from `V59`: only while a rotation set with members is in effect, with a drawn
//! turn naming the operator from its boundary rather than its mirror, from the turns an activation
//! finds held, with rotation on from it until a mirrored frontier carries the registry's turn state,
//! from the turn that frontier names, and from a passed proposal that shows a turn the engine has
//! not seen.

use super::*;

use common::Client;
use engine_core::eip712::B256;
use engine_core::election::RegistryInstallation;
use engine_core::emissions::TokenUnits;
use engine_core::turns::{DAY_MS, RegistryRotationSet, RegistryTurn, SetMember, TurnReason, boundary};

#[test]
fn votes_govern_only_once_a_rotation_set_is_in_effect() {
    let mut h = voting(ProtocolVersion::V59, TTL);
    switch_rotation(&mut h, true);
    // Rotation is on but no set is in effect, so no member can propose: the operator keeps its
    // bounded commands, and so does the node.
    assert_eq!(h.operate(house_cap(&h, ETH, -1)).last(), Some(&Event::AdminApplied));
    let policy = engine_core::access::AccessPolicy { source: AccountId([0x5a; 20]), max_age_ms: 60_000 };
    assert_eq!(h.cmd(Command::Admin(AdminCommand::SetAccessPolicy(policy))).last(), Some(&Event::AdminApplied));
    let command = margin(&h, BTC, 1_500);
    refused(&pass(&mut h, 1, true, command), "no rotation set is in effect");
    // A set adopted but not yet in effect changes nothing until its time.
    let effective_at = h.ts + NOTICE_MS;
    adopt_set(&mut h, 1, effective_at);
    h.advance_to(effective_at - 1);
    assert_eq!(h.operate(house_cap(&h, ETH, -2)).last(), Some(&Event::AdminApplied));
    h.advance(1);
    rejected(&h.operate(house_cap(&h, ETH, -3)), "only a passed vote changes this");
    let command = margin(&h, BTC, 1_500);
    let events = pass(&mut h, 2, true, command);
    assert!(events.iter().any(|e| matches!(e, Event::RestrictionApplied { proposal: 2, .. })), "{events:?}");
}

#[test]
fn votes_govern_only_while_the_set_in_effect_has_members() {
    let mut h = governed();
    rejected(&h.operate(house_cap(&h, ETH, -1)), "only a passed vote changes this");
    // A close with no eligible candidate adopts an empty set: once it takes effect no one can propose.
    let effective_at = h.ts + NOTICE_MS;
    adopt_members(&mut h, 2, effective_at, Vec::new());
    h.advance_to(effective_at);
    assert_eq!(h.operate(house_cap(&h, ETH, -1)).last(), Some(&Event::AdminApplied));
    // A proposal opened under the earlier set can still pass, and applies.
    let command = margin(&h, BTC, 1_500);
    let events = pass(&mut h, 1, true, command);
    assert!(events.iter().any(|e| matches!(e, Event::RestrictionApplied { proposal: 1, .. })), "{events:?}");
}

#[test]
fn a_drawn_turn_names_the_operator_from_its_boundary_not_its_mirror() {
    for accepted in [true, false] {
        let mut h = voting(ProtocolVersion::V59, TTL);
        let now = h.ts;
        adopt_set(&mut h, 1, now);
        switch_rotation(&mut h, true);
        // Seven days on, the set's key has its notice by the next turn; at once, it does not.
        if accepted {
            h.advance(NOTICE_MS);
        }
        let turn = next_turn(&h);
        let begins = boundary(turn).expect("in range");
        h.advance_to(begins - DAY_MS);
        let events = draw(&mut h, turn, 1);
        assert_eq!(matches!(events.last(), Some(Event::TurnAssigned { .. })), accepted, "{events:?}");
        assert_eq!(matches!(events.last(), Some(Event::TurnRefused { .. })), !accepted, "{events:?}");
        // The switch turns off before the turn starts: until its boundary no turn names the operator.
        switch_rotation(&mut h, false);
        h.advance_to(begins - 1);
        assert_eq!(h.operate(house_cap(&h, ETH, -1)).last(), Some(&Event::AdminApplied), "accepted {accepted}");
        h.advance(1);
        rejected(&h.operate(house_cap(&h, ETH, -2)), "only a passed vote changes this");
        assert_eq!(h.engine.governance().turns_from, Some(begins));
    }
}

#[test]
fn an_activation_takes_when_turns_name_the_operator_from_the_turns_the_log_holds() {
    for accepted in [true, false] {
        // Under `V56`, with rotation on, a vote passes `V59`'s activation, which applies seven days on.
        let mut h = voting(ProtocolVersion::V56, TTL);
        switch_rotation(&mut h, true);
        let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 };
        let logged = h.ts;
        pass(&mut h, 1, false, activation.clone());
        // The turn that starts first after the activation is drawn the day before.
        let turn = (0..).find(|turn| boundary(*turn).expect("in range") > logged + NOTICE_MS).expect("a later turn");
        let begins = boundary(turn).expect("in range");
        // A set mirrored a week before the turn gives its key notice, one mirrored at the draw does not.
        if !accepted {
            h.advance_to(begins - DAY_MS);
        }
        let now = h.ts;
        adopt_set(&mut h, 1, now);
        h.advance_to(begins - DAY_MS);
        let events = draw(&mut h, turn, 1);
        assert_eq!(matches!(events.last(), Some(Event::TurnAssigned { .. })), accepted, "{events:?}");
        switch_rotation(&mut h, false);
        h.advance_to(logged + NOTICE_MS);
        assert_eq!(apply(&mut h, 1, activation).last(), Some(&Event::AdminApplied));
        // An accepted turn names the operator from its boundary. A refused one left no boundary, so
        // with sets held rotation counts as on until a frontier carries the registry's turn state.
        assert_eq!(h.engine.governance().turns_from, accepted.then_some(begins));
        assert!(h.engine.governance().turns_drawn);
        rejected(&h.operate(house_cap(&h, ETH, -1)), "only a passed vote changes this");
        // Before the boundary the registry names no turn.
        assert_eq!(frontier(&mut h, Some(None)), []);
        assert!(!h.engine.governance().turns_drawn);
        assert_eq!(h.operate(house_cap(&h, ETH, -1)).last(), Some(&Event::AdminApplied));
        h.advance_to(begins);
        if !accepted {
            frontier(&mut h, Some(Some(begins)));
        }
        rejected(&h.operate(house_cap(&h, ETH, -2)), "only a passed vote changes this");
    }
}

#[test]
fn the_registry_names_a_turn_refused_before_v56_at_the_next_frontier() {
    let (mut h, begins) = refused_before_v56();
    // The log holds no record of the refused turn, but holds the set it was drawn from: until the
    // registry's state arrives, rotation counts as on.
    assert_eq!(h.engine.governance().turns_from, None);
    rejected(&h.operate(house_cap(&h, ETH, -1)), "only a passed vote changes this");
    // A frontier cannot name a turn that starts after its block.
    let events = h.cmd(Command::StakingProgress {
        block: h.log.len() as u64,
        block_hash: B256::repeat_byte(0x57),
        block_time: Some(begins - 1),
        named_turn: Some(Some(begins)),
    });
    assert!(
        matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("frontier")),
        "{events:?}"
    );
    assert_eq!(frontier(&mut h, Some(Some(begins))), []);
    assert_eq!((h.engine.governance().turns_from, h.engine.governance().turns_drawn), (Some(begins), false));
    rejected(&h.operate(house_cap(&h, ETH, -2)), "only a passed vote changes this");
    // An installation ends the turns, and the operator governs again.
    install(&mut h);
    assert_eq!(h.engine.governance().turns_from, None);
    assert_eq!(h.operate(house_cap(&h, ETH, -3)).last(), Some(&Event::AdminApplied));
    // Before `V59` no frontier carries the registry's turn state.
    for named_turn in [Some(None), Some(Some(0))] {
        let mut h = governed_under(ProtocolVersion::V56, TTL);
        let events = frontier(&mut h, named_turn);
        assert!(
            matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("frontier")),
            "{events:?}"
        );
    }
}

#[test]
fn a_frontier_naming_no_turn_ends_what_the_activation_could_not_see() {
    // Under `V55` a set is adopted and the switch turns on and off before any draw, then `V59`.
    let mut h = voting(ProtocolVersion::V55, TTL);
    let now = h.ts;
    adopt_set(&mut h, 1, now);
    switch_rotation(&mut h, true);
    switch_rotation(&mut h, false);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 });
    rejected(&h.operate(house_cap(&h, ETH, -1)), "only a passed vote changes this");
    // A frontier carrying none of the registry's turn state resolves nothing.
    assert_eq!(frontier(&mut h, None), []);
    rejected(&h.operate(house_cap(&h, ETH, -1)), "only a passed vote changes this");
    // The registry names no turn: the operator's bounded commands apply.
    assert_eq!(frontier(&mut h, Some(None)), []);
    assert_eq!(h.operate(house_cap(&h, ETH, -1)).last(), Some(&Event::AdminApplied));
}

#[test]
fn a_proposal_the_registry_passes_shows_a_turn_the_engine_has_not_seen() {
    // Under `V55` a turn is drawn and refused, the switch turns off before its boundary, and `V59`
    // activates; a frontier before the boundary finds no turn named.
    let mut h = voting(ProtocolVersion::V55, TTL);
    let now = h.ts;
    adopt_set(&mut h, 1, now);
    switch_rotation(&mut h, true);
    let turn = next_turn(&h);
    let begins = boundary(turn).expect("in range");
    h.advance_to(begins - DAY_MS);
    assert!(matches!(draw(&mut h, turn, 1).last(), Some(Event::TurnRefused { .. })));
    switch_rotation(&mut h, false);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 });
    h.admin(AdminCommand::SetGovernance(GovernanceConfig { vote: VOTE, restriction_ttl_ms: TTL }));
    assert_eq!(frontier(&mut h, Some(None)), []);
    // Past the boundary the registry passes a proposal, which it does only while a turn names the
    // operator: before the next frontier brings its state, the pass applies, and votes govern from then.
    h.advance_to(begins + 60_000);
    let command = margin(&h, BTC, 1_500);
    let events = pass(&mut h, 1, true, command);
    assert!(events.iter().any(|e| matches!(e, Event::RestrictionApplied { proposal: 1, .. })), "{events:?}");
    assert_eq!(h.engine.governance().turns_from, Some(h.ts));
    rejected(&h.operate(house_cap(&h, ETH, -1)), "only a passed vote changes this");
}

/// Under `V55` a set is mirrored too late for its first turn, which is credited and refused, and the
/// switch turns off two days on, when the registry's drawn turn names the operator; then `V59` and
/// the vote contract. The harness and the turn's boundary.
fn refused_before_v56() -> (Harness, Millis) {
    let mut h = voting(ProtocolVersion::V55, TTL);
    let now = h.ts;
    adopt_set(&mut h, 1, now);
    switch_rotation(&mut h, true);
    let turn = next_turn(&h);
    let begins = boundary(turn).expect("in range");
    h.advance_to(begins - DAY_MS);
    assert!(matches!(draw(&mut h, turn, 1).last(), Some(Event::TurnRefused { .. })));
    h.advance(2 * DAY_MS);
    switch_rotation(&mut h, false);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 });
    h.admin(AdminCommand::SetGovernance(GovernanceConfig { vote: VOTE, restriction_ttl_ms: TTL }));
    (h, begins)
}

/// The rotation set's one member, under the harness operator's key, so a turn naming it leaves the
/// key in effect.
pub(super) fn member() -> SetMember {
    SetMember { candidate: AccountId([0xc1; 20]), admin_key: Client::operator().account, weight: TokenUnits(1) }
}

/// Mirrors set `set` of `member`, in effect from `effective_at`.
pub(super) fn adopt_set(h: &mut Harness, set: u32, effective_at: Millis) {
    adopt_members(h, set, effective_at, vec![member()]);
}

/// Mirrors set `set` of `members`, in effect from `effective_at`.
pub(super) fn adopt_members(h: &mut Harness, set: u32, effective_at: Millis, members: Vec<SetMember>) {
    let (block, chain_ref) = (h.log.len() as u64, format!("0xset:{set}"));
    let adoption =
        RegistryRotationSet { set, epoch: u64::from(set), effective_at, quorum: true, members, block, chain_ref };
    let events = h.cmd(Command::AdoptRotationSet(adoption));
    assert!(events.iter().any(|e| matches!(e, Event::RotationSetAdopted { .. })), "{events:?}");
}

/// Mirrors a staking frontier at now, carrying the registry's turn state there when `named_turn` is
/// `Some`: the boundary of the turn naming the operator, or none.
pub(super) fn frontier(h: &mut Harness, named_turn: Option<Option<Millis>>) -> Vec<Event> {
    let (block, block_hash, block_time) = (h.log.len() as u64, B256::repeat_byte(0x57), Some(h.ts));
    h.cmd(Command::StakingProgress { block, block_hash, block_time, named_turn })
}

/// The first turn whose draw deadline, a day before its boundary, is after now.
pub(super) fn next_turn(h: &Harness) -> u64 {
    (0..).find(|turn| boundary(*turn).expect("in range") - DAY_MS > h.ts).expect("a later turn")
}

/// Mirrors `turn` drawn from `set`, naming `member`.
pub(super) fn draw(h: &mut Harness, turn: u64, set: u32) -> Vec<Event> {
    let SetMember { candidate, admin_key, .. } = member();
    let (reason, block, chain_ref) = (TurnReason::Draw, h.log.len() as u64, format!("0xturn:{turn}"));
    h.cmd(Command::AssignTurn(RegistryTurn { turn, set, position: 0, reason, candidate, admin_key, block, chain_ref }))
}

/// Mirrors the timelock installing `member`, which ends the drawn turns.
pub(super) fn install(h: &mut Harness) {
    let SetMember { candidate, admin_key, .. } = member();
    let (block, chain_ref) = (h.log.len() as u64, format!("0xinstall:{}", h.log.len()));
    let events = h.cmd(Command::InstallOperator(RegistryInstallation { candidate, admin_key, block, chain_ref }));
    assert!(events.iter().any(|e| matches!(e, Event::OperatorInstalled { .. })), "{events:?}");
}

/// `market`'s baseline listing with its house cap moved by `delta` bps: a change the operator signs
/// with no notice, which only a vote applies while votes govern.
pub(super) fn house_cap(h: &Harness, market: MarketId, delta: i64) -> AdminCommand {
    let spec = h.engine.baseline_spec(market).expect("listed");
    AdminCommand::UpsertMarket(MarketSpec { house_cap_bps: spec.house_cap_bps + delta, ..spec })
}
