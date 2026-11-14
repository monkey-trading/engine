//! Attributing a range of the log to the members and releases that sequenced it, the node's own check
//! at a handover, and what a member's quote and the operator's question bind.

use super::*;

const A: B256 = B256::repeat_byte(0xa1);
const B: B256 = B256::repeat_byte(0xb1);
const C: B256 = B256::repeat_byte(0xc1);
/// A's member on another release, as after an upgrade.
const A2: B256 = B256::repeat_byte(0xa2);
/// B's member on another release.
const B2: B256 = B256::repeat_byte(0xb2);

#[test]
fn a_range_names_the_member_in_office_at_its_start_and_each_that_took_a_turn_in_it() {
    let takings = takings(0, &[(10, A), (20, B), (30, B), (40, C)]);
    let names = |after, through| takings.sequenced(after, through).unwrap();
    // Before the first `TurnTaken` no one is recorded.
    assert_eq!(names(None, 9), Sequencers::default());
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

#[test]
fn records_that_start_past_a_range_attribute_none_of_it() {
    // A replay from a snapshot sees the commands from its base on, so it cannot say who sequenced
    // them until a `TurnTaken` it saw.
    let takings = takings(50, &[(60, B)]);
    assert_eq!(takings.sequenced(None, 70), None, "a range from genesis");
    assert_eq!(takings.sequenced(Some(10), 70), None, "a range starting before the records");
    assert_eq!(takings.sequenced(Some(49), 70), None, "a range whose member took its turn before the base");
    assert_eq!(takings.sequenced(Some(59), 70), Some(sequencers(&[B])), "a range from a recorded TurnTaken");
    // A first command that is itself a `TurnTaken` names who sequenced from it.
    assert_eq!(takings_after_base(50).sequenced(Some(49), 55), Some(sequencers(&[B])));
    // No command seen: complete from the next, which only a log with none yet can attribute.
    assert_eq!(Takings::default().complete_from(0).sequenced(None, 5), Some(Sequencers::default()));
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
    // With nothing verified yet, the range runs from genesis.
    assert_eq!(own_check(None, &recorded, head(39), (C, &member(C))), Some(head(39)));
    assert_eq!(own_check(None, &recorded, head(39), (A2, &member(A))), None);
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

/// The member that runs `release`: `A2` and `B2` are A's and B's other releases.
fn candidate(release: B256) -> AccountId {
    AccountId([release.0[0] & 0xf0; 20])
}

/// The member that runs `release`, as the candidates a node's operator key sits for.
fn member(release: B256) -> BTreeSet<AccountId> {
    BTreeSet::from([candidate(release)])
}

/// Who sequenced a range in which each of `releases` took a turn by its member.
fn sequencers(releases: &[B256]) -> Sequencers {
    Sequencers {
        candidates: releases.iter().map(|release| candidate(*release)).collect(),
        releases: releases.iter().copied().collect(),
    }
}

/// A checkpoint at `seq`.
fn head(seq: u64) -> VerifiedHead {
    VerifiedHead { seq, link: B256::repeat_byte(seq as u8), root: B256::repeat_byte(!(seq as u8)) }
}
