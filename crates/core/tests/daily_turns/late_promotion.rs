//! `V71`: a promotion the schedule finalised after its turn's nominal end applies when it is mirrored,
//! while no later turn is in effect, and from `V72` whichever way the engine took that turn's draw. The
//! promoted member's key signs and its `TurnTaken` is accepted;
//! a promotion a later turn superseded, and any other reassignment of an ended turn, stays refused,
//! and an earlier version refuses them all.

use super::*;

/// `rotating` under `version`, with set 1 mirrored and `turn` drawn for member 0 and taken a minute
/// after its boundary. Returns the clock at ten minutes past the turn's end, with no later turn drawn.
fn after_end(version: ProtocolVersion, later: bool) -> (Harness, u64) {
    let mut h = rotating();
    h.admin(AdminCommand::ActivateProtocol { version });
    assert!(matches!(adopt(&mut h, 1, members()).last(), Some(Event::RotationSetAdopted { set: 1, .. })));
    let turn = next_turn(h.ts + 8 * DAY_MS);
    let begins = boundary(turn).unwrap();
    h.advance_to(begins - DAY_MS);
    assert!(matches!(assign(&mut h, turn, 0, 0, TurnReason::Draw).last(), Some(Event::TurnAssigned { .. })));
    if later {
        let drawn = assign(&mut h, turn + 1, 1, 0, TurnReason::Draw);
        assert!(matches!(drawn.last(), Some(Event::TurnAssigned { .. })), "{drawn:?}");
    }
    h.advance_to(begins + 60_000);
    assert_eq!(signed_by(&mut h, SEEDS[0], true), Some(Event::AdminApplied));
    let taken = h.cmd(Command::TurnTaken { turn, release: release(1) });
    assert!(matches!(taken.last(), Some(Event::TurnTaken { .. })), "{taken:?}");
    h.advance_to(begins + DAY_MS + 600_000);
    h.cmd(Command::Tick);
    (h, turn)
}

#[test]
fn a_promotion_mirrored_after_its_turn_ended_applies_from_its_mirror() {
    let (mut h, turn) = after_end(ProtocolVersion::V71, false);
    assert_eq!(operator_key(&h), key(0), "no later turn was drawn, so the first member's key stays");
    let mirrored = h.ts;
    let events = assign(&mut h, turn, 2, 1, TurnReason::Promotion);
    assert!(
        matches!(events.last(), Some(Event::TurnAssigned { applies_at, position: 1, .. }) if *applies_at == mirrored),
        "{events:?}"
    );
    assert!(matches!(signed_by(&mut h, SEEDS[0], false), Some(Event::AdminRejected { .. })));
    assert_eq!(signed_by(&mut h, SEEDS[2], false), Some(Event::AdminApplied));
    let taken = h.cmd(Command::TurnTaken { turn, release: release(2) });
    assert_eq!(taken.last(), Some(&Event::TurnTaken { turn, candidate: CANDIDATES[2], release: release(2) }));
}

#[test]
fn before_v71_every_assignment_of_an_ended_turn_is_refused() {
    let (mut h, turn) = after_end(ProtocolVersion::V69, false);
    turn_refused(&assign(&mut h, turn, 2, 1, TurnReason::Promotion), "ended before its assignment was mirrored");
    assert_eq!(operator_key(&h), key(0));
}

#[test]
fn a_promotion_a_later_turn_in_effect_superseded_is_refused() {
    let (mut h, turn) = after_end(ProtocolVersion::V71, true);
    assert_eq!(h.engine.daily_turns().in_effect.map(|in_effect| in_effect.turn), Some(turn + 1));
    turn_refused(&assign(&mut h, turn, 2, 1, TurnReason::Promotion), "ended before its assignment was mirrored");
    assert_eq!(operator_key(&h), key(1), "the later turn's key stays");
}

#[test]
fn a_draw_or_a_skip_of_an_ended_turn_is_refused_from_v71() {
    for reason in [TurnReason::Draw, TurnReason::Skip] {
        let (mut h, turn) = after_end(ProtocolVersion::V71, false);
        turn_refused(&assign(&mut h, turn, 2, 1, reason), "ended before its assignment was mirrored");
        assert_eq!(operator_key(&h), key(0), "{reason:?}");
    }
}

#[test]
fn a_late_promotion_still_needs_a_key_its_set_names() {
    let (mut h, turn) = after_end(ProtocolVersion::V71, false);
    let stranger = RegistryTurn {
        turn,
        set: 1,
        position: 1,
        reason: TurnReason::Promotion,
        candidate: CANDIDATES[2],
        admin_key: AccountId([0x99; 20]),
        block: h.log.len() as u64,
        chain_ref: "0xturn:stranger".into(),
    };
    turn_refused(&h.cmd(Command::AssignTurn(stranger)), "does not name candidate");
    assert_eq!(operator_key(&h), key(0));
}

#[test]
fn a_late_promotion_still_needs_seven_days_notice_at_its_mirror() {
    let (mut h, turn) = after_end(ProtocolVersion::V71, false);
    // A day before the promotion, a set mirrored the third member under another key.
    let mut moved = members();
    moved[2].admin_key = AccountId([0x98; 20]);
    h.advance(DAY_MS);
    let effective_at = h.ts;
    adopt_effective(&mut h, 2, moved, effective_at);
    h.advance(DAY_MS);
    let promotion = RegistryTurn {
        turn,
        set: 2,
        position: 1,
        reason: TurnReason::Promotion,
        candidate: CANDIDATES[2],
        admin_key: AccountId([0x98; 20]),
        block: h.log.len() as u64,
        chain_ref: "0xturn:new-key".into(),
    };
    turn_refused(&h.cmd(Command::AssignTurn(promotion)), "less than seven days before the turn applies");
    assert_eq!(operator_key(&h), key(0));
}

/// `rotating` under `version`, with set 1, then set 2 (the same members) in effect from the draw
/// deadline of a turn drawn from set 1 for member 0 and applied, then, ten minutes after the turn's end,
/// set 3 mirrored. Returns the turn.
fn pruned(version: ProtocolVersion) -> (Harness, u64) {
    let mut h = rotating();
    h.admin(AdminCommand::ActivateProtocol { version });
    assert!(matches!(adopt(&mut h, 1, members()).last(), Some(Event::RotationSetAdopted { set: 1, .. })));
    let turn = next_turn(h.ts + 8 * DAY_MS);
    let begins = boundary(turn).unwrap();
    h.advance_to(begins - DAY_MS);
    assert!(matches!(assign(&mut h, turn, 0, 0, TurnReason::Draw).last(), Some(Event::TurnAssigned { .. })));
    let effective_at = h.ts;
    adopt_effective(&mut h, 2, members(), effective_at);
    h.advance_to(begins + 60_000);
    assert_eq!(signed_by(&mut h, SEEDS[0], true), Some(Event::AdminApplied));
    h.advance_to(begins + DAY_MS + 600_000);
    h.cmd(Command::Tick);
    let events = adopt(&mut h, 3, members());
    assert!(matches!(events.last(), Some(Event::RotationSetAdopted { set: 3, .. })), "{events:?}");
    (h, turn)
}

#[test]
fn the_set_that_drew_the_turn_in_effect_stays_for_its_late_promotion_from_v71() {
    let sets = |h: &Harness| h.engine.daily_turns().sets.keys().copied().collect::<Vec<_>>();
    let (mut h, turn) = pruned(ProtocolVersion::V69);
    assert_eq!(sets(&h), [2, 3], "set 1 is pruned as before");
    turn_refused(&assign(&mut h, turn, 2, 1, TurnReason::Promotion), "ended before its assignment was mirrored");

    let (mut h, turn) = pruned(ProtocolVersion::V71);
    assert_eq!(sets(&h), [1, 2, 3], "set 1 stays while its turn is in effect");
    let events = assign(&mut h, turn, 2, 1, TurnReason::Promotion);
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
    // A later turn in effect supersedes the turn, and the next adoption lets set 1 go.
    let next = next_turn(h.ts);
    h.advance_to(boundary(next).unwrap());
    assert!(matches!(assign_from(&mut h, 3, next, 1, 0, TurnReason::Draw).last(), Some(Event::TurnAssigned { .. })));
    h.cmd(Command::Tick);
    adopt(&mut h, 4, members());
    assert_eq!(sets(&h), [2, 3, 4]);
}

/// `rotating` under `V71` with set 1, optionally an earlier turn drawn and in effect, then a turn whose
/// draw is refused for a key no set names, and the clock ten minutes after that turn's end. Returns
/// that turn.
fn refused_draw(earlier: bool) -> (Harness, u64) {
    let mut h = rotating();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V71 });
    assert!(matches!(adopt(&mut h, 1, members()).last(), Some(Event::RotationSetAdopted { set: 1, .. })));
    let first = next_turn(h.ts + 8 * DAY_MS);
    h.advance_to(boundary(first).unwrap() - DAY_MS);
    if earlier {
        assert!(matches!(assign(&mut h, first, 0, 0, TurnReason::Draw).last(), Some(Event::TurnAssigned { .. })));
    }
    let turn = first + 1;
    let stranger = RegistryTurn {
        turn,
        set: 1,
        position: 0,
        reason: TurnReason::Draw,
        candidate: CANDIDATES[1],
        admin_key: AccountId([0x99; 20]),
        block: h.log.len() as u64,
        chain_ref: "0xturn:stranger-draw".into(),
    };
    turn_refused(&h.cmd(Command::AssignTurn(stranger)), "does not name candidate");
    h.advance_to(boundary(turn).unwrap() + DAY_MS + 600_000);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.daily_turns().in_effect.map(|in_effect| in_effect.turn), earlier.then_some(first));
    (h, turn)
}

#[test]
fn a_late_promotion_applies_with_no_turn_in_effect_or_an_earlier_one() {
    for earlier in [false, true] {
        let (mut h, turn) = refused_draw(earlier);
        let mirrored = h.ts;
        let events = assign(&mut h, turn, 2, 1, TurnReason::Promotion);
        assert!(
            matches!(events.last(), Some(Event::TurnAssigned { applies_at, .. }) if *applies_at == mirrored),
            "{earlier}: {events:?}"
        );
        assert_eq!(signed_by(&mut h, SEEDS[2], false), Some(Event::AdminApplied), "{earlier}");
    }
}

#[test]
fn a_stall_keeps_the_set_of_the_turn_in_effect_and_forgets_the_obsolete_ones() {
    let (mut h, _) = pruned(ProtocolVersion::V71);
    // Nobody draws for ten days while closes adopt sets: set 2 is past drawing once set 3 has been in
    // effect two days, but set 1 still names the turn in effect.
    h.advance(10 * DAY_MS);
    h.cmd(Command::Tick);
    let events = adopt(&mut h, 4, members());
    assert!(matches!(events.last(), Some(Event::RotationSetAdopted { set: 4, .. })), "{events:?}");
    let sets: Vec<u32> = h.engine.daily_turns().sets.keys().copied().collect();
    assert_eq!(sets, [1, 3, 4]);
}

/// `rotating` under `version` with sets 1 and 2, a turn drawn from set 1 and in effect, and set 3, which
/// takes effect just after the next turn's draw deadline. That turn's draw from set 2 is refused for a key no
/// set names, and with `later` the draw of the turn after it from set 3. The staking mirror then stalls for ten
/// days from the last turn's boundary, and set 4, adopted after the draws, takes effect seven days on, as a
/// chain adoption does, and is mirrored on its resumption. Returns the first refused turn.
fn refused_from_another_set(version: ProtocolVersion, later: bool) -> (Harness, u64) {
    let mut h = rotating();
    h.admin(AdminCommand::ActivateProtocol { version });
    assert!(matches!(adopt(&mut h, 1, members()).last(), Some(Event::RotationSetAdopted { set: 1, .. })));
    assert!(matches!(adopt(&mut h, 2, members()).last(), Some(Event::RotationSetAdopted { set: 2, .. })));
    let first = next_turn(h.ts + 8 * DAY_MS);
    let turn = first + 1;
    let deadline = boundary(turn).unwrap() - DAY_MS;
    let events = adopt_effective(&mut h, 3, members(), deadline + 1);
    assert!(matches!(events.last(), Some(Event::RotationSetAdopted { set: 3, .. })), "{events:?}");
    h.advance_to(boundary(first).unwrap() - DAY_MS);
    assert!(matches!(assign(&mut h, first, 0, 0, TurnReason::Draw).last(), Some(Event::TurnAssigned { .. })));
    h.advance_to(boundary(first).unwrap() + 60_000);
    assert_eq!(signed_by(&mut h, SEEDS[0], true), Some(Event::AdminApplied));
    turn_refused(&stranger_draw(&mut h, 2, turn), "does not name candidate");
    if later {
        turn_refused(&stranger_draw(&mut h, 3, turn + 1), "does not name candidate");
    }
    let last = if later { turn + 1 } else { turn };
    h.advance_to(boundary(last).unwrap() + 10 * DAY_MS);
    h.cmd(Command::Tick);
    let events = adopt_effective(&mut h, 4, members(), boundary(last).unwrap() + 7 * DAY_MS);
    assert!(matches!(events.last(), Some(Event::RotationSetAdopted { set: 4, .. })), "{events:?}");
    (h, turn)
}

/// Mirrors the draw of `turn` from `set` for a key no set names.
fn stranger_draw(h: &mut Harness, set: u32, turn: u64) -> Vec<Event> {
    let stranger = RegistryTurn {
        turn,
        set,
        position: 0,
        reason: TurnReason::Draw,
        candidate: CANDIDATES[1],
        admin_key: AccountId([0x99; 20]),
        block: h.log.len() as u64,
        chain_ref: format!("0xturn:stranger-draw:{turn}"),
    };
    h.cmd(Command::AssignTurn(stranger))
}

fn sets(h: &Harness) -> Vec<u32> {
    h.engine.daily_turns().sets.keys().copied().collect()
}

#[test]
fn the_set_of_a_refused_draw_stays_for_its_late_promotion_from_v72() {
    let (mut h, turn) = refused_from_another_set(ProtocolVersion::V71, false);
    assert_eq!(sets(&h), [1, 4], "sets 2 and 3 are pruned as before");
    let events = assign_from(&mut h, 2, turn, 2, 1, TurnReason::Promotion);
    turn_refused(&events, "is not among the sets mirrored");

    let (mut h, turn) = refused_from_another_set(ProtocolVersion::V72, false);
    assert_eq!(sets(&h), [1, 2, 4], "set 2 stays while its refused turn can be promoted");
    let events = assign_from(&mut h, 2, turn, 2, 1, TurnReason::Promotion);
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
    assert_eq!(signed_by(&mut h, SEEDS[2], false), Some(Event::AdminApplied));
}

#[test]
fn a_later_turn_in_effect_releases_the_set_of_a_refused_draw() {
    let (mut h, turn) = refused_from_another_set(ProtocolVersion::V72, false);
    assert_eq!(h.engine.daily_turns().refused.get(&turn), Some(&2));
    let next = next_turn(h.ts);
    h.advance_to(boundary(next).unwrap());
    assert!(matches!(assign_from(&mut h, 4, next, 1, 0, TurnReason::Draw).last(), Some(Event::TurnAssigned { .. })));
    h.cmd(Command::Tick);
    assert!(h.engine.daily_turns().refused.is_empty());
    h.advance_to(boundary(next).unwrap() + 10 * DAY_MS);
    h.cmd(Command::Tick);
    adopt_effective(&mut h, 5, members(), boundary(next).unwrap() + 7 * DAY_MS);
    assert_eq!(sets(&h), [4, 5]);
}

#[test]
fn a_refused_later_draw_does_not_replace_the_set_kept_for_an_earlier_turn() {
    let (mut h, turn) = refused_from_another_set(ProtocolVersion::V72, true);
    let kept = h.engine.daily_turns().refused.iter().map(|(t, s)| (*t, *s)).collect::<Vec<_>>();
    assert_eq!(kept, [(turn, 2), (turn + 1, 3)]);
    assert_eq!(sets(&h), [1, 2, 3, 4], "both refused turns can still be promoted");
    let events = assign_from(&mut h, 2, turn, 2, 1, TurnReason::Promotion);
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
}

#[test]
fn a_refusal_of_a_turn_before_the_one_in_effect_records_nothing() {
    let (mut h, turn) = after_end(ProtocolVersion::V72, true);
    turn_refused(&assign(&mut h, turn, 2, 1, TurnReason::Skip), "ended before its assignment was mirrored");
    assert!(h.engine.daily_turns().refused.is_empty());
}

#[test]
fn an_installation_ends_the_record_of_refused_draws() {
    let (mut h, _) = refused_from_another_set(ProtocolVersion::V72, false);
    assert!(!h.engine.daily_turns().refused.is_empty());
    let (block, chain_ref) = (h.log.len() as u64, "0xoff".to_owned());
    h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: false, block, chain_ref });
    h.advance(DAY_MS);
    let (block, chain_ref) = (h.log.len() as u64, "0xinstall".to_owned());
    let installation = engine_core::election::RegistryInstallation {
        candidate: AccountId([0xb9; 20]),
        admin_key: AccountId([0x6a; 20]),
        block,
        chain_ref,
    };
    let events = h.cmd(Command::InstallOperator(installation));
    assert!(matches!(events.last(), Some(Event::OperatorInstalled { .. })), "{events:?}");
    assert!(h.engine.daily_turns().refused.is_empty());
}
