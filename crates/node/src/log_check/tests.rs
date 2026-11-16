//! Attributing a range of the log to the members and releases that sequenced it, the node's own check
//! at a handover, and what a member's quote and the operator's question bind.

use super::*;

const A: B256 = B256::repeat_byte(0xa1);
const B: B256 = B256::repeat_byte(0xb1);
const C: B256 = B256::repeat_byte(0xc1);
const D: B256 = B256::repeat_byte(0xd1);
const E: B256 = B256::repeat_byte(0xe1);
/// A's member on another release, as after an upgrade.
const A2: B256 = B256::repeat_byte(0xa2);
/// B's member on another release.
const B2: B256 = B256::repeat_byte(0xb2);

/// A turn has begun from its boundary to the next, at every turn length: a short-turns build's
/// five-minute turns as well as a day's.
#[test]
fn a_turn_has_begun_from_its_boundary_until_the_next() {
    // The turn starting at 22:30 UTC on 2026-11-06, a boundary at either length.
    let at: Millis = 1_794_004_200_000;
    let turn = u64::try_from((at - engine_core::turns::BOUNDARY_OFFSET_MS) / engine_core::turns::TURN_MS).unwrap();
    let start = engine_core::turns::boundary(turn).unwrap();
    assert_eq!(start, at);
    assert_eq!(turn_begun_by(start), Some(turn));
    assert_eq!(turn_begun_by(start - 1), Some(turn - 1));
    assert_eq!(turn_begun_by(start + engine_core::turns::TURN_MS - 1), Some(turn));
    assert_eq!(turn_begun_by(start + engine_core::turns::TURN_MS), Some(turn + 1));
    assert_eq!(turn_begun_by(engine_core::turns::BOUNDARY_OFFSET_MS - 1), None, "before the first boundary");
}

#[test]
fn a_range_names_the_member_in_office_at_its_start_and_each_that_took_a_turn_in_it() {
    let takings = takings(0, &[(10, A), (20, B), (30, B), (40, C)]);
    let names = |after, through| takings.sequenced(after, through).unwrap();
    // No record names who sequenced before the first `TurnTaken`: the operator before the turns.
    assert_eq!(takings.sequenced(None, 9), None);
    assert_eq!(takings.sequenced(Some(5), 15), None);
    assert_eq!(names(None, 15), sequencers(&[A]));
    // From a head inside B's holding, B alone, until C takes its turn.
    assert_eq!(names(Some(19), 39), sequencers(&[B]));
    assert_eq!(names(Some(25), 39), sequencers(&[B]));
    assert_eq!(names(Some(25), 40), sequencers(&[B, C]));
    // A head before B's `TurnTaken` leaves the command after it to A.
    assert_eq!(names(Some(18), 35), sequencers(&[A, B]));
    // A head at the command before a `TurnTaken`, as a handover's is, leaves nothing to the one before.
    assert_eq!(names(Some(39), 45), sequencers(&[C]));
}

/// A promoted member sequences from its promotion, before the finalised mirror of it and its own
/// `TurnTaken`, which a failure before then never logs: each promotion mirrored after the `TurnTaken`
/// in office, accepted or refused, names its candidate in a range through the mirror.
#[test]
fn a_range_names_each_member_promoted_after_the_turn_taken_in_office() {
    // A takes its turn at 10. B, promoted, sequences from 21 and fails before its `TurnTaken`; C,
    // promoted past it, sequences from 31; the mirrors of B's promotion (refused) and C's (accepted)
    // follow at 35 and 36, and C's `TurnTaken` at 40.
    let mut failed = takings(0, &[(10, A)]);
    failed.command(35, &[promotion(B, false)]);
    failed.command(36, &[promotion(C, true)]);
    failed.command(40, &[turn_taken(C)]);
    let names = |after, through| failed.sequenced(after, through).unwrap();
    let candidates = |releases: &[B256]| releases.iter().map(|release| candidate(*release)).collect::<BTreeSet<_>>();
    assert_eq!(names(Some(20), 45).candidates, candidates(&[A, B, C]), "C's check names B");
    assert!(names(Some(20), 45).include(candidate(B), B2), "so B's member verifies none of it on any release");
    assert_eq!(own_check(Some(head(20)), &failed, head(45), (B2, &member(B))), Some(head(20)));
    assert_eq!(names(Some(20), 36).candidates, candidates(&[A, B, C]), "a range through the mirrors names them");
    assert_eq!(names(Some(39), 45), sequencers(&[C]), "from C's `TurnTaken`, the promotions before it name none");
    let mut pruned = failed.clone();
    pruned.prune(39);
    assert_eq!(pruned.sequenced(Some(39), 45), Some(sequencers(&[C])));
    // B, surviving, logs its `TurnTaken` at 30 after its promotion's mirror at 25: its commands from 21 on
    // are its own in a range through either.
    let mut survived = takings(0, &[(10, A)]);
    survived.command(25, &[promotion(B, true)]);
    survived.command(30, &[turn_taken(B)]);
    assert_eq!(survived.sequenced(Some(20), 27).unwrap().candidates, candidates(&[A, B]));
    assert_eq!(survived.sequenced(Some(20), 32).unwrap().candidates, candidates(&[A, B]));
}

/// A skip's member, like a promoted one, sequences from its take before the mirror and its `TurnTaken`
/// (#596): a range through the mirror names it, accepted or refused.
#[test]
fn a_range_names_each_member_skipped_to_after_the_turn_taken_in_office() {
    let candidates = |releases: &[B256]| releases.iter().map(|release| candidate(*release)).collect::<BTreeSet<_>>();
    for accepted in [true, false] {
        let mut skipped = takings(0, &[(10, A)]);
        skipped.command(35, &[reassignment(B, accepted, TurnReason::Skip)]);
        assert_eq!(skipped.sequenced(Some(20), 40).unwrap().candidates, candidates(&[A, B]), "accepted {accepted}");
        assert!(skipped.sequenced(Some(20), 40).unwrap().include(candidate(B), B2));
    }
    // A draw's mirror names no one past the `TurnTaken` in office.
    let mut drawn = takings(0, &[(10, A)]);
    drawn.command(35, &[reassignment(B, true, TurnReason::Draw)]);
    assert_eq!(drawn.sequenced(Some(20), 40).unwrap().candidates, candidates(&[A]));
}

/// Finality can hold a promotion's mirror past the range its member sequenced in: a promotion mirrored
/// past the range names its member when its turn is the one in office at the range's start or a later
/// one, whether or not a `TurnTaken` in the range names that turn; a late mirror of a promotion in an
/// earlier turn, whose member's commands all precede the `TurnTaken` in office, names none.
#[test]
fn a_promotion_mirrored_past_the_range_names_its_member_from_the_turn_in_office_on() {
    let taken = |turn, release| Event::TurnTaken { turn, candidate: candidate(release), release };
    let promoted = |turn, release| Event::TurnAssigned {
        turn,
        position: 1,
        reason: TurnReason::Promotion,
        candidate: candidate(release),
        admin_key: candidate(release),
        applies_at: 0,
    };
    // A takes turn 2 at 10. B, promoted in turn 2, sequences 21 to 25; turn 3's drawn member never takes
    // it, and D, promoted in turn 3, sequences 26 to 30; C takes turn 4 at 31, and a check covers the
    // range through 34. The mirrors land past it: B's at 35, D's at 36, and at 37 a late one of E's
    // promotion in turn 1, whose commands all precede A's `TurnTaken`.
    let mut late = Takings::default();
    late.command(10, &[taken(2, A)]);
    late.command(31, &[taken(4, C)]);
    late.command(35, &[promoted(2, B)]);
    late.command(36, &[promoted(3, D)]);
    late.command(37, &[promoted(1, E)]);
    let names = late.sequenced(Some(20), 34).unwrap();
    assert!(names.candidates.contains(&candidate(B)), "B's mirror past the range named none of its commands");
    assert!(names.candidates.contains(&candidate(D)), "a promotion in a turn no `TurnTaken` names was missed");
    assert!(!names.candidates.contains(&candidate(E)), "an earlier turn's promotion named a member of none of it");
    for member_of in [B, D] {
        let own = own_check(Some(head(20)), &late, head(34), (A2, &member(member_of)));
        assert_eq!(own, Some(head(20)), "{member_of}'s member checked its own work");
    }
    assert_eq!(own_check(Some(head(20)), &late, head(34), (E, &member(E))), Some(head(34)), "E sequenced none");
}

/// A promotion mirrored past the range names its member only once its turn had begun by the range's
/// last command: C takes turn 2 and a check covers the range through 34; D, promoted in turn 3, which
/// began after seq 34, sequenced none of it, whenever its mirror lands.
#[test]
fn a_promotion_in_a_turn_that_began_after_the_range_names_no_member_of_it() {
    let begin = |turn: u64| engine_core::turns::boundary(turn).unwrap();
    let taken = |turn, release| Event::TurnTaken { turn, candidate: candidate(release), release };
    let promoted = |turn, release| Event::TurnAssigned {
        turn,
        position: 1,
        reason: TurnReason::Promotion,
        candidate: candidate(release),
        admin_key: candidate(release),
        applies_at: 0,
    };
    let mut late = Takings::default();
    late.stamped(10, begin(2) + 1);
    late.command(10, &[taken(2, C)]);
    late.stamped(34, begin(3) - 1);
    // D's turn began at 35, and its mirror landed at 36.
    late.stamped(35, begin(3));
    late.stamped(36, begin(3) + 5);
    late.command(36, &[promoted(3, D)]);
    let names = late.sequenced(Some(20), 34).unwrap();
    assert!(!names.candidates.contains(&candidate(D)), "a turn that began after the range named its member");
    assert_eq!(own_check(Some(head(20)), &late, head(34), (D, &member(D))), Some(head(34)), "D checked C's holding");
    // A range through a command stamped in turn 3 names D.
    let names = late.sequenced(Some(20), 36).unwrap();
    assert!(names.candidates.contains(&candidate(D)), "a turn that began by the range's end named no member");
    assert_eq!(own_check(Some(head(20)), &late, head(36), (D, &member(D))), Some(head(20)));
    // The stamps survive a prune at a verified head inside the range.
    late.prune(20);
    assert!(!late.sequenced(Some(20), 34).unwrap().candidates.contains(&candidate(D)));
}

/// A start's own check also reads the schedule, which a promotion or skip within the finality window may
/// have changed before the log mirrors it: it counts for nothing when the schedule promoted or skipped
/// to its member after the in-office assignment, or when it read no turn as early as the in-office one.
#[test]
fn the_nodes_own_check_counts_for_nothing_when_the_schedule_promoted_its_member_in_the_range() {
    // A takes turn 2 at 10; B, promoted in it, sequenced 21 to 30 with no mirror in the log.
    let mut recorded = Takings::default();
    recorded.command(10, &[Event::TurnTaken { turn: 2, candidate: candidate(A), release: A }]);
    let at = |position, release, reassigned| Assigned { turn: 2, position, candidate: candidate(release), reassigned };
    let own = |read: &Reassigned| own_check_read(Some(head(20)), &recorded, head(34), (B2, &member(B)), Some(read));
    let read = Reassigned { assigned: vec![at(0, A, false), at(1, B, true)] };
    assert_eq!(own(&read), Some(head(20)), "B's member checked its own work");
    // Without a record of the promotion, the log's records alone count it.
    assert_eq!(own_check_read(Some(head(20)), &recorded, head(34), (B2, &member(B)), None), Some(head(34)));
    // Another member's promotion, or a draw of this one, says nothing of it.
    assert_eq!(own(&Reassigned { assigned: vec![at(0, B, false), at(1, C, true)] }), Some(head(34)));
    // A reassignment in a turn that began after the range's last command sequenced none of it.
    let begin = |turn: u64| engine_core::turns::boundary(turn).unwrap();
    recorded.stamped(10, begin(2) + 1);
    recorded.stamped(34, begin(3) - 1);
    let later = Reassigned { assigned: vec![at(0, A, false), Assigned { turn: 3, ..at(1, B, true) }] };
    let own = |read: &Reassigned| own_check_read(Some(head(20)), &recorded, head(34), (B2, &member(B)), Some(read));
    assert_eq!(own(&later), Some(head(34)), "a turn that began after the range named its member");
    recorded.stamped(40, begin(3) + 1);
    let own = |read: &Reassigned| own_check_read(Some(head(20)), &recorded, head(40), (B2, &member(B)), Some(read));
    assert_eq!(own(&later), Some(head(20)), "a turn that had begun by the range's end did not");
}

/// A member the schedule promoted or skipped to after the in-office holder's assignment may have
/// sequenced part of the range before the log mirrored it, in the holder's turn or a later one; one
/// reassigned earlier in the holder's turn ended before the holder's `TurnTaken`, and a draw is no
/// reassignment. With no assignment of the holder's in its turn, each of that turn's counts.
#[test]
fn reassignments_after_the_in_office_assignment_name_their_members() {
    let at =
        |turn, position, release, reassigned| Assigned { turn, position, candidate: candidate(release), reassigned };
    // Turn 2's draw named A; B was promoted at position 1, C at 3, whose `TurnTaken` is in office, and D
    // at 4. Turn 3's draw named E, and A was skipped to at 2.
    let assigned = [
        at(2, 0, A, false),
        at(2, 1, B, true),
        at(2, 3, C, true),
        at(2, 4, D, true),
        at(3, 0, E, false),
        at(3, 2, A, true),
    ];
    let in_office = |release| Sequencers {
        candidates: BTreeSet::new(),
        releases: BTreeSet::new(),
        in_office: (2, candidate(release)),
    };
    let names = |releases: &[B256]| releases.iter().map(|release| candidate(*release)).collect::<BTreeSet<_>>();
    assert_eq!(in_office(C).reassigned(&assigned), names(&[D, A]), "after C's assignment, and in turn 3");
    assert_eq!(in_office(E).reassigned(&assigned), names(&[B, C, D, A]), "E holds no assignment of turn 2");
}

#[test]
fn records_that_start_past_a_range_attribute_none_of_it() {
    // A replay from a snapshot sees the commands from its base on, so it cannot say who sequenced
    // them until a `TurnTaken` it saw.
    let takings = takings(50, &[(60, B)]);
    assert_eq!(takings.sequenced(None, 55), None, "no verified head and no TurnTaken since the base");
    assert_eq!(takings.sequenced(None, 70), Some(sequencers(&[B])), "no verified head: the holding it saw");
    assert_eq!(takings.sequenced(Some(10), 70), None, "a range starting before the records");
    assert_eq!(takings.sequenced(Some(49), 70), None, "a range whose member took its turn before the base");
    assert_eq!(takings.sequenced(Some(59), 70), Some(sequencers(&[B])), "a range from a recorded TurnTaken");
    // A first command that is itself a `TurnTaken` names who sequenced from it.
    assert_eq!(takings_after_base(50).sequenced(Some(49), 55), Some(sequencers(&[B])));
    // No command seen: complete from the next, with no `TurnTaken` to attribute anything by.
    assert_eq!(Takings::default().complete_from(0).sequenced(None, 5), None);
    assert_eq!(Takings::default().complete_from(8).sequenced(Some(7), 9), None);
    assert_eq!(Takings::default().complete_from(8).sequenced(Some(3), 9), None);
    // The node's own check of a range it cannot attribute keeps the head it had.
    assert_eq!(own_check(Some(head(49)), &takings, head(55), (C, &member(C))), Some(head(49)));
}

#[test]
fn pruning_at_a_verified_head_keeps_the_member_in_office_after_it() {
    let mut takings = takings(0, &[(10, A), (20, B), (30, B), (40, C)]);
    takings.prune(25);
    assert_eq!(takings.sequenced(Some(25), 45), Some(sequencers(&[B, C])));
    assert_eq!(takings.last(), Some((40, C)));
    // A `TurnTaken` right after the head is the one in office from it.
    let mut takings = takings_at_handover();
    takings.prune(39);
    assert_eq!(takings.sequenced(Some(39), 45), Some(sequencers(&[C])));
}

#[test]
fn the_nodes_own_check_counts_only_when_neither_its_member_nor_its_release_sequenced_the_range() {
    let recorded = takings(0, &[(10, A), (20, B), (30, B), (40, C)]);
    let previous = Some(head(19));
    // B handed over at 39 to C: C's replay verified B's range.
    assert_eq!(own_check(previous, &recorded, head(39), (C, &member(C))), Some(head(39)));
    // A release that sequenced part of the range verifies nothing for it.
    assert_eq!(own_check(previous, &recorded, head(39), (B, &member(C))), previous);
    // Nor does the member that sequenced it on another release, as after its upgrade, whether or not
    // its start was flagged as one.
    assert_eq!(own_check(previous, &recorded, head(39), (B2, &member(B2))), previous);
    assert_eq!(own_check(Some(head(9)), &recorded, head(19), (A2, &member(A))), Some(head(9)));
    // Nor does a head at or below the one verified, or one the records cannot attribute.
    assert_eq!(own_check(Some(head(39)), &recorded, head(30), (C, &member(C))), Some(head(39)));
    assert_eq!(own_check(previous, &takings(25, &[(30, B)]), head(39), (C, &member(C))), previous);
    // A member's upgrade before any turn checks nothing: no record names the operator before them.
    let before_turns = takings(0, &[]);
    assert_eq!(own_check(None, &before_turns, head(9), (A2, &member(A))), None);
    // With nothing verified, the range is the holding of the latest `TurnTaken`: B's.
    assert_eq!(own_check(None, &recorded, head(39), (C, &member(C))), Some(head(39)));
    assert_eq!(own_check(None, &recorded, head(39), (A2, &member(A))), Some(head(39)));
    assert_eq!(own_check(None, &recorded, head(39), (B2, &member(B2))), None);
}

#[test]
fn a_missing_head_leaves_the_latest_holding_to_check_not_every_turn_since_genesis() {
    // Every member has taken turns; the head was lost, as when a host keeps the sealed one from a start
    // or a release that sealed none takes the keys.
    let recorded = takings(0, &[(10, A), (20, B), (30, C), (40, A), (50, B)]);
    assert_eq!(recorded.sequenced(None, 55), Some(sequencers(&[B])));
    assert_eq!(recorded.sequenced(None, 45), Some(sequencers(&[A])));
    // B's holding hands over to C, whose start checks it and so sets the head again.
    assert_eq!(own_check(None, &recorded, head(55), (C, &member(C))), Some(head(55)));
    // At a boundary B holds on through, any member but B can be asked.
    let held = takings(0, &[(10, A), (20, B), (30, C), (40, A), (50, B), (60, B)]);
    let excluded = held.sequenced(None, 61).unwrap();
    assert!(!excluded.include(candidate(A), A) && !excluded.include(candidate(C), C), "{excluded:?}");
    assert!(excluded.include(candidate(B), B2));
}

#[test]
fn a_verifier_is_excluded_by_its_member_or_its_release() {
    let excluded = sequencers(&[A, B]);
    assert!(excluded.include(candidate(A), C), "a member that sequenced the range, on another release");
    assert!(excluded.include(candidate(C), B), "a release that sequenced the range");
    assert!(!excluded.include(candidate(C), C));
}

#[test]
fn the_report_data_and_the_question_bind_every_part_of_the_statement_and_the_challenge() {
    let domain = CheckpointDomain { chain_id: 31_337, vault: AccountId([7; 20]) };
    let statement = Statement {
        signer_epoch: 2,
        boundary: engine_core::turns::boundary(20_755).unwrap(),
        turn: 20_755,
        position: 1,
        set: 1,
        head: head(39),
    };
    let variants = [
        (CheckpointDomain { chain_id: 1, ..domain }, [1; 32], statement),
        (domain, [3; 32], statement),
        (domain, [1; 32], Statement { signer_epoch: 3, ..statement }),
        (domain, [1; 32], Statement { boundary: 0, ..statement }),
        (domain, [1; 32], Statement { turn: 20_756, ..statement }),
        (domain, [1; 32], Statement { position: 0, ..statement }),
        (domain, [1; 32], Statement { set: 2, ..statement }),
        (domain, [1; 32], Statement { head: head(40), ..statement }),
        (domain, [1; 32], Statement { head: VerifiedHead { link: B, ..head(39) }, ..statement }),
        (domain, [1; 32], Statement { head: VerifiedHead { root: B, ..head(39) }, ..statement }),
    ];
    let base = report_data(domain, &[1; 32], &statement, &[2; 32]);
    assert_ne!(report_data(domain, &[1; 32], &statement, &[4; 32]), base, "another config");
    let asked = ask_digest(domain, &[1; 32], &statement);
    for (index, (domain, nonce, statement)) in variants.iter().enumerate() {
        assert_ne!(report_data(*domain, nonce, statement, &[2; 32]), base, "change {index} left the report data");
        assert_ne!(ask_digest(*domain, nonce, statement), asked, "change {index} left the question's digest");
    }
    // The question is signed under its own tag, so its digest is no quote's report data.
    assert_ne!(asked.0, base[..32], "the question's digest is the report data");
}

// ── Helpers ──

/// Records from the command at `from` on, with a `TurnTaken` of each release, by its member, at each
/// seq.
fn takings(from: u64, taken: &[(u64, B256)]) -> Takings {
    let mut takings = Takings::default();
    takings.command(from, &[]);
    for (seq, release) in taken {
        takings.command(*seq, &[turn_taken(*release)]);
    }
    takings
}

/// Records from a snapshot whose first command after the base, at `first`, is B's `TurnTaken`.
fn takings_after_base(first: u64) -> Takings {
    let mut takings = Takings::default();
    takings.command(first, &[turn_taken(B)]);
    takings
}

/// B's holding, handed over to C, whose `TurnTaken` follows the handed-over head at 39.
fn takings_at_handover() -> Takings {
    takings(0, &[(10, A), (20, B), (40, C)])
}

/// A `TurnTaken` under `release`, by its member.
fn turn_taken(release: B256) -> Event {
    Event::TurnTaken { turn: 1, candidate: candidate(release), release }
}

/// The mirror of a promotion of `release`'s member, accepted or refused.
fn promotion(release: B256, accepted: bool) -> Event {
    reassignment(release, accepted, TurnReason::Promotion)
}

/// The mirror of a reassignment for `cause` to `release`'s member, accepted or refused.
fn reassignment(release: B256, accepted: bool, cause: TurnReason) -> Event {
    let (candidate, admin_key) = (candidate(release), candidate(release));
    match accepted {
        true => Event::TurnAssigned { turn: 1, position: 1, reason: cause, candidate, admin_key, applies_at: 0 },
        false => Event::TurnRefused { turn: 1, position: 1, cause, candidate, admin_key, reason: "ended".into() },
    }
}

/// The member that runs `release`: `A2` and `B2` are A's and B's other releases.
fn candidate(release: B256) -> AccountId {
    AccountId([release.0[0] & 0xf0; 20])
}

/// The member that runs `release`, as the candidates a node's operator key sits for.
fn member(release: B256) -> BTreeSet<AccountId> {
    BTreeSet::from([candidate(release)])
}

/// Who sequenced a range in which each of `releases` took turn 1 by its member, the first in office at
/// its start.
fn sequencers(releases: &[B256]) -> Sequencers {
    Sequencers {
        candidates: releases.iter().map(|release| candidate(*release)).collect(),
        releases: releases.iter().copied().collect(),
        in_office: (1, candidate(releases[0])),
    }
}

/// A checkpoint at `seq`.
fn head(seq: u64) -> VerifiedHead {
    VerifiedHead { seq, link: B256::repeat_byte(seq as u8), root: B256::repeat_byte(!(seq as u8)) }
}
