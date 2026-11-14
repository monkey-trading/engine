//! Attributing a range of the log to the releases that sequenced it, the node's own check at a
//! handover, and the report data a member's quote binds.

use super::*;

const A: B256 = B256::repeat_byte(0xa1);
const B: B256 = B256::repeat_byte(0xb1);
const C: B256 = B256::repeat_byte(0xc1);

#[test]
fn a_range_names_the_release_in_office_at_its_start_and_each_that_took_a_turn_in_it() {
    let takings = takings(0, &[(10, A), (20, B), (30, B), (40, C)]);
    let names = |after, through| takings.sequenced(after, through).unwrap().into_iter().collect::<Vec<_>>();
    // Before the first `TurnTaken` no release is recorded.
    assert_eq!(names(None, 9), Vec::<B256>::new());
    assert_eq!(names(None, 15), [A]);
    // From a head inside B's holding, B alone, until C takes its turn.
    assert_eq!(names(Some(19), 39), [B]);
    assert_eq!(names(Some(25), 39), [B]);
    assert_eq!(names(Some(25), 40), [B, C]);
    // A head before B's `TurnTaken` leaves the command after it to A.
    assert_eq!(names(Some(18), 35), [A, B]);
    // A head at the command before a `TurnTaken`, as a handover's is, leaves nothing to the one before.
    assert_eq!(names(Some(39), 45), [C]);
}

#[test]
fn records_that_start_past_a_range_attribute_none_of_it() {
    // A replay from a snapshot sees the commands from its base on.
    let takings = takings(50, &[(60, B)]);
    assert_eq!(takings.sequenced(None, 70), None, "a range from genesis");
    assert_eq!(takings.sequenced(Some(10), 70), None, "a range starting before the records");
    assert_eq!(takings.sequenced(Some(49), 70), Some(BTreeSet::from([B])));
    // No command seen: complete from the next.
    assert_eq!(Takings::default().complete_from(0, None).sequenced(None, 5), Some(BTreeSet::new()));
    assert_eq!(Takings::default().complete_from(8, None).sequenced(Some(3), 9), None);
}

#[test]
fn a_snapshot_start_names_the_release_the_engine_records_in_office_before_its_first_command() {
    // A took its turn before the snapshot's base: the engine's record names it from the first command.
    let takings = takings_after(50, Some(A), &[(60, B)]);
    assert_eq!(takings.sequenced(Some(49), 55), Some(BTreeSet::from([A])));
    assert_eq!(takings.sequenced(Some(49), 70), Some(BTreeSet::from([A, B])));
    assert_eq!(takings.sequenced(Some(59), 70), Some(BTreeSet::from([B])));
    // A first command that is itself a `TurnTaken` is the one in office from it.
    let mut first_taken = Takings::default();
    first_taken.command(50, &[Event::TurnTaken { turn: 1, candidate: AccountId([1; 20]), release: B }], Some(B));
    assert_eq!(first_taken.sequenced(Some(49), 55), Some(BTreeSet::from([B])));
    // No command seen past the snapshot: the engine's record names the release in office from the next.
    let none_seen = Takings::default().complete_from(50, Some(A));
    assert_eq!(none_seen.sequenced(Some(49), 55), Some(BTreeSet::from([A])));
    // The node's own check of such a range counts only from another release.
    assert_eq!(own_check(Some(head(49)), &takings, head(55), A), Some(head(49)));
    assert_eq!(own_check(Some(head(49)), &takings, head(55), C), Some(head(55)));
}

#[test]
fn pruning_at_a_verified_head_keeps_the_release_in_office_after_it() {
    let mut takings = takings(0, &[(10, A), (20, B), (30, B), (40, C)]);
    takings.prune(25);
    assert_eq!(takings.sequenced(Some(25), 45), Some(BTreeSet::from([B, C])));
    assert_eq!(takings.last(), Some((40, C)));
    // A `TurnTaken` right after the head is the one in office from it.
    let mut takings = takings_at_handover();
    takings.prune(39);
    assert_eq!(takings.sequenced(Some(39), 45), Some(BTreeSet::from([C])));
}

#[test]
fn the_nodes_own_check_counts_only_when_its_release_sequenced_none_of_the_range() {
    let recorded = takings(0, &[(10, A), (20, B), (30, B), (40, C)]);
    let previous = Some(head(19));
    // B handed over at 39 to C: C's replay verified B's range.
    assert_eq!(own_check(previous, &recorded, head(39), C), Some(head(39)));
    // A release that sequenced part of the range verifies nothing for it.
    assert_eq!(own_check(previous, &recorded, head(39), B), previous);
    // Nor does a head at or below the one verified, or one the records cannot attribute.
    assert_eq!(own_check(Some(head(39)), &recorded, head(30), C), Some(head(39)));
    assert_eq!(own_check(previous, &takings(25, &[(30, B)]), head(39), C), previous);
    // With nothing verified yet, the range runs from genesis.
    assert_eq!(own_check(None, &recorded, head(39), C), Some(head(39)));
    assert_eq!(own_check(None, &recorded, head(39), A), None);
}

#[test]
fn the_report_data_binds_every_part_of_the_statement_the_challenge_and_the_config() {
    let domain = CheckpointDomain { chain_id: 31_337, vault: AccountId([7; 20]) };
    let statement = Statement {
        signer_epoch: 2,
        boundary: engine_core::turns::boundary(20_755).unwrap(),
        turn: 20_755,
        position: 1,
        set: 1,
        head: head(39),
    };
    let base = report_data(domain, &[1; 32], &statement, &[2; 32]);
    let changed = [
        report_data(CheckpointDomain { chain_id: 1, ..domain }, &[1; 32], &statement, &[2; 32]),
        report_data(domain, &[3; 32], &statement, &[2; 32]),
        report_data(domain, &[1; 32], &statement, &[4; 32]),
        report_data(domain, &[1; 32], &Statement { signer_epoch: 3, ..statement }, &[2; 32]),
        report_data(domain, &[1; 32], &Statement { boundary: 0, ..statement }, &[2; 32]),
        report_data(domain, &[1; 32], &Statement { turn: 20_756, ..statement }, &[2; 32]),
        report_data(domain, &[1; 32], &Statement { position: 0, ..statement }, &[2; 32]),
        report_data(domain, &[1; 32], &Statement { set: 2, ..statement }, &[2; 32]),
        report_data(domain, &[1; 32], &Statement { head: head(40), ..statement }, &[2; 32]),
        report_data(domain, &[1; 32], &Statement { head: VerifiedHead { link: B, ..head(39) }, ..statement }, &[2; 32]),
        report_data(domain, &[1; 32], &Statement { head: VerifiedHead { root: B, ..head(39) }, ..statement }, &[2; 32]),
    ];
    for (index, other) in changed.iter().enumerate() {
        assert_ne!(*other, base, "change {index} left the report data as it was");
    }
}

// ── Helpers ──

/// Records from the command at `from` on, with a `TurnTaken` of each release at each seq.
fn takings(from: u64, taken: &[(u64, B256)]) -> Takings {
    takings_after(from, None, taken)
}

/// `takings`, from a first command after which the engine records `in_office` as the last release to
/// take a turn, as a replay from a snapshot past that `TurnTaken` starts.
fn takings_after(from: u64, in_office: Option<B256>, taken: &[(u64, B256)]) -> Takings {
    let mut takings = Takings::default();
    takings.command(from, &[], in_office);
    for (seq, release) in taken {
        let event = Event::TurnTaken { turn: *seq, candidate: AccountId([1; 20]), release: *release };
        takings.command(*seq, &[event], Some(*release));
    }
    takings
}

/// B's holding, handed over to C, whose `TurnTaken` follows the handed-over head at 39.
fn takings_at_handover() -> Takings {
    takings(0, &[(10, A), (20, B), (40, C)])
}

/// A checkpoint at `seq`.
fn head(seq: u64) -> VerifiedHead {
    VerifiedHead { seq, link: B256::repeat_byte(seq as u8), root: B256::repeat_byte(!(seq as u8)) }
}
