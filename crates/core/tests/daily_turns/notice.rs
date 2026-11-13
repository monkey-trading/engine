//! `V53`: a member's seven days' notice runs from the first of the unbroken run of mirrored sets that
//! named its candidate with its key. A set mirrored late, already in effect on chain, keeps the notice
//! of each member it names unchanged; a carried notice still under seven days refuses, and one of
//! exactly seven days applies; the notice survives the pruning of the set that began it, and V53's
//! activation carries it through the sets the log kept under V51.

use super::*;

/// A key `CANDIDATES[1]` re-registered under, which no earlier set named.
const NEW_KEY: AccountId = AccountId([0x77; 20]);

/// `rotating`, under `version`.
fn rotating_under(version: ProtocolVersion) -> Harness {
    let mut h = rotating();
    if version > ProtocolVersion::V51 {
        h.admin(AdminCommand::ActivateProtocol { version });
    }
    h
}

/// Mirrors `turn` from set `set` for `CANDIDATES[1]` under `NEW_KEY`, returning its events.
fn assign_new_key(h: &mut Harness, set: u32, turn: u64) -> Vec<Event> {
    let (block, chain_ref) = (h.log.len() as u64, format!("0xturn:new-key:{turn}"));
    h.cmd(Command::AssignTurn(RegistryTurn {
        turn,
        set,
        position: 0,
        reason: TurnReason::Draw,
        candidate: CANDIDATES[1],
        admin_key: NEW_KEY,
        block,
        chain_ref,
    }))
}

/// Asserts the last of `events` accepted a turn.
fn assigned(events: &[Event]) {
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
}

#[test]
fn from_v53_a_set_mirrored_late_keeps_the_notice_of_each_member_it_names_unchanged() {
    for version in [ProtocolVersion::V51, ProtocolVersion::V53] {
        let mut h = rotating_under(version);
        adopt(&mut h, 1, members());
        let first = h.ts;
        // Weeks on, after a custody outage, set 2 is mirrored already in effect on chain, a day before
        // the turn drawn from it: the first member unchanged, the second under another key, the third
        // dropped.
        h.advance(21 * DAY_MS);
        let mut second = members();
        second[1].admin_key = NEW_KEY;
        second.truncate(2);
        let effective_at = h.ts - DAY_MS;
        adopt_effective(&mut h, 2, second, effective_at);
        let logged = h.ts;
        let turn = next_turn(h.ts + DAY_MS);
        h.advance_to(boundary(turn).unwrap() - DAY_MS);
        let events = assign_from(&mut h, 2, turn, 0, 0, TurnReason::Draw);
        if version == ProtocolVersion::V53 {
            assigned(&events);
        } else {
            turn_refused(&events, "less than seven days before the turn applies");
        }
        // The key set 2 names afresh has had no seven days, whichever version.
        turn_refused(&assign_new_key(&mut h, 2, turn + 1), "set 2 was logged at");
        let recorded = &h.engine.daily_turns().sets[&2];
        let carried = (version == ProtocolVersion::V53).then_some(first);
        assert_eq!(recorded.noticed.get(&CANDIDATES[0]).copied(), carried, "the unchanged member's notice was lost");
        assert!(!recorded.noticed.contains_key(&CANDIDATES[1]), "a member under another key kept its notice");

        // The next set carries both notices on; a member back after a set that dropped it starts afresh.
        let mut third = members();
        third[1].admin_key = NEW_KEY;
        adopt(&mut h, 3, third);
        let recorded = &h.engine.daily_turns().sets[&3];
        assert_eq!(recorded.noticed.get(&CANDIDATES[0]).copied(), carried, "the notice did not carry on");
        let carried_key = (version == ProtocolVersion::V53).then_some(logged);
        assert_eq!(recorded.noticed.get(&CANDIDATES[1]).copied(), carried_key, "the new key's notice did not carry on");
        assert!(!recorded.noticed.contains_key(&CANDIDATES[2]), "a member back after a gap kept its notice");
    }
}

#[test]
fn a_carried_notice_applies_at_exactly_seven_days_and_not_before() {
    let mut h = rotating_under(ProtocolVersion::V53);
    // Set 1 is mirrored exactly seven days before a turn's boundary, set 2 a day later naming its
    // members unchanged.
    let turn = next_turn(h.ts + 8 * DAY_MS);
    h.advance_to(boundary(turn).unwrap() - NOTICE_MS);
    adopt(&mut h, 1, members());
    h.advance(DAY_MS);
    let effective_at = h.ts;
    adopt_effective(&mut h, 2, members(), effective_at);
    // The turn a day earlier applies six days after the notice began: refused on the carried notice.
    turn_refused(&assign_from(&mut h, 2, turn - 1, 0, 0, TurnReason::Draw), "names the key the log has named since");
    // The turn itself applies seven days to the millisecond after it.
    assigned(&assign_from(&mut h, 2, turn, 1, 0, TurnReason::Draw));
}

#[test]
fn a_carried_notice_survives_the_pruning_of_the_set_that_began_it() {
    let mut h = rotating_under(ProtocolVersion::V53);
    adopt(&mut h, 1, members());
    let first = h.ts;
    // A month on, set 2 is mirrored two days after it took effect, so no turn yet to end draws from
    // set 1 and it is forgotten; set 3 follows the same way a week later, and set 2 goes too.
    for set in [2, 3] {
        h.advance(if set == 2 { 30 * DAY_MS } else { 7 * DAY_MS });
        let effective_at = h.ts - 2 * DAY_MS;
        adopt_effective(&mut h, set, members(), effective_at);
        let kept: Vec<_> = h.engine.daily_turns().sets.keys().copied().collect();
        assert_eq!(kept, [set], "the set before {set} is still kept");
        assert_eq!(h.engine.daily_turns().sets[&set].noticed.get(&CANDIDATES[0]).copied(), Some(first));
    }
    // A turn drawn from set 3 within a day of its mirror applies on the notice set 1 began.
    let turn = next_turn(h.ts + DAY_MS);
    h.advance_to(boundary(turn).unwrap() - DAY_MS);
    assigned(&assign_from(&mut h, 3, turn, 0, 0, TurnReason::Draw));
}

#[test]
fn v53s_activation_carries_the_notice_through_the_sets_v51_kept() {
    let mut h = rotating_under(ProtocolVersion::V51);
    adopt(&mut h, 1, members());
    let first = h.ts;
    // Under V51, set 2 is mirrored late, already in effect, naming the members unchanged.
    h.advance(8 * DAY_MS);
    let effective_at = h.ts - DAY_MS;
    adopt_effective(&mut h, 2, members(), effective_at);
    assert!(h.engine.daily_turns().sets[&2].noticed.is_empty(), "V51 carried a notice");
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V53 });
    assert_eq!(h.engine.daily_turns().sets[&2].noticed.get(&CANDIDATES[0]).copied(), Some(first));
    assert_eq!(h.engine.daily_turns().sets[&1].noticed, Default::default(), "the first kept set carried a notice");
    // A turn drawn from set 2 a day on applies on the notice set 1 began.
    let turn = next_turn(h.ts + DAY_MS);
    h.advance_to(boundary(turn).unwrap() - DAY_MS);
    assigned(&assign_from(&mut h, 2, turn, 0, 0, TurnReason::Draw));
}
