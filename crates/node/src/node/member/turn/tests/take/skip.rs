//! A member's turn under the latest agreed block's reassignments (#596): it takes a turn a skip names
//! it for before the finalised schedule shows the skip, follows rather than takes one a skip or a
//! promotion moved past it, holds one it already holds keys for, asks the operator whose head is
//! newest first, and follows that operator every other time a stint fails.

use super::*;

use crate::handover::assigned::tests::{FakeAssignments, skip};
use crate::handover::assigned::{Assigned, Reason};
use crate::handover::discovery::Registered;
use crate::node::member::turn::Holding;

/// The generation a skip of this member's turn holds.
const SKIPPED: u64 = 21;

/// An operator `byte` names, with its release, hostname and handover endpoint.
fn operator_of(byte: u8) -> HandoverSource {
    HandoverSource {
        candidate: AccountId([byte; 20]),
        release: [byte; 32],
        api_hostname: format!("{byte}.test"),
        handover_endpoint: format!("{byte}.test:7443"),
    }
}

impl Fixture {
    /// The member, reading `assignments` as the latest agreed block's.
    fn member_reading(&self, assignments: &Arc<FakeAssignments>) -> Member<'_, Registry, Heads, Time> {
        let mut member = self.member();
        member.fallback.assignments = Some(assignments.clone());
        member
    }
}

/// A skip of this member's turn, 12, at position 2 to `release`, run by candidate `byte`.
fn skip_of_own(release: [u8; 32], byte: u8) -> Assigned {
    skip(12, 2, byte, release, SKIPPED)
}

#[tokio::test]
async fn a_skip_naming_this_member_takes_the_turn_before_the_finalised_schedule_shows_it() {
    let fixture = Fixture::start("member-skip-take", true).await;
    // The finalised schedule draws turn 12 for another release; the latest block's skip names this one.
    fixture.time.set(OWN + 1);
    let assignments = Arc::new(FakeAssignments::default());
    assignments.set(Some(skip_of_own(fixture.release().id(), 0x0c)));
    let mut member = fixture.member_reading(&assignments);
    match member.holding().await.unwrap() {
        Holding::Take(turn) => {
            assert_eq!((turn.turn, turn.generation, turn.release), (12, SKIPPED, fixture.release().id()));
            assert_eq!((turn.candidate, turn.admin_key), (AccountId([0x0c; 20]), AccountId([0x0d; 20])));
            assert_eq!(turn.boundary, OWN, "the turn's boundary stays the draw's");
        }
        _ => panic!("the skip's member did not take its turn"),
    }
    // With no reader, the finalised schedule alone decides: another member's turn, which it follows.
    let mut member = fixture.member();
    assert!(matches!(member.holding().await.unwrap(), Holding::Follow(Some(turn)) if turn.turn == 12));
}

#[tokio::test]
async fn a_skip_this_member_gave_away_or_already_holds_keys_for_does_not_take_it_again() {
    let fixture = Fixture::start("member-skip-held", true).await;
    fixture.time.set(OWN + 1);
    let assignments = Arc::new(FakeAssignments::default());
    assignments.set(Some(skip_of_own(fixture.release().id(), 0x0c)));
    let mut member = fixture.member_reading(&assignments);
    member.handed_over(Handed { generation: Some(SKIPPED), reached: None, head: None });
    assert!(matches!(member.holding().await.unwrap(), Holding::Follow(_)), "it took back the turn it handed over");
    let sealer = MockSealer { launch_digest: DIGEST };
    KeyStore::create(&fixture.keys, &sealer, fixture.cfg.file_sha256, SKIPPED, KeySet::generate().unwrap()).unwrap();
    let mut member = fixture.member_reading(&assignments);
    assert!(matches!(member.holding().await.unwrap(), Holding::Hold), "it took the keys it holds");
}

/// A member a skip or a promotion moved past follows, keys or not: the registry no longer names it, so a
/// start as their holder would be refused (`check_rotation`), as an incumbent's past its boundary is.
#[tokio::test]
async fn a_member_a_skip_or_a_promotion_moved_past_follows_whether_or_not_it_holds_the_turns_keys() {
    for reason in [Reason::Skip, Reason::Promotion] {
        let fixture = Fixture::start(&format!("member-skipped-{reason:?}"), true).await;
        fixture.registry.draw(fixture.release().id());
        fixture.time.set(OWN + 1);
        let assignments = Arc::new(FakeAssignments::default());
        assignments.set(Some(Assigned { reason, ..skip_of_own([0x0e; 32], 0x0e) }));
        let mut member = fixture.member_reading(&assignments);
        match member.holding().await.unwrap() {
            Holding::Follow(Some(turn)) => assert_eq!(turn.release, fixture.release().id(), "{reason:?}"),
            _ => panic!("{reason:?}: a member moved past took its turn"),
        }
        fixture.seal_own_turn();
        let mut member = fixture.member_reading(&assignments);
        assert!(matches!(member.holding().await.unwrap(), Holding::Follow(Some(_))), "{reason:?}: it held");
    }
}

#[tokio::test]
async fn a_draw_at_the_latest_block_leaves_the_finalised_schedule_to_decide() {
    let fixture = Fixture::start("member-skip-draw", true).await;
    fixture.registry.draw(fixture.release().id());
    fixture.time.set(OWN + 1);
    let assignments = Arc::new(FakeAssignments::default());
    assignments.set(Some(Assigned { reason: Reason::Draw, ..skip_of_own([0x0e; 32], 0x0e) }));
    let mut member = fixture.member_reading(&assignments);
    assert!(matches!(member.holding().await.unwrap(), Holding::Take(turn) if turn.generation == GENERATION));
}

#[tokio::test]
async fn a_skips_take_asks_the_operator_whose_head_is_newest_first_and_binds_the_skip() {
    let fixture = Fixture::start("member-skip-sources", true).await;
    fixture.registry.source(operator_of(0x0a));
    fixture.time.set(OWN + 1);
    let assignments = Arc::new(FakeAssignments::default());
    // B took turn 12 late and was skipped; its heads are the newest. A, the turn's source, drained.
    let assigned = Assigned { head_release: Some([0x0b; 32]), ..skip_of_own(fixture.release().id(), 0x0c) };
    assignments.set(Some(assigned));
    *assignments.members.lock().unwrap() = vec![
        Registered { source: operator_of(0x0a), held: None },
        Registered { source: operator_of(0x0b), held: Some(0) },
        Registered { source: operator_of(0x0c), held: Some(2) },
    ];
    let member = fixture.member_reading(&assignments);
    let Holding::Take(turn) = fixture.member_reading(&assignments).holding().await.unwrap() else {
        panic!("the skip's member did not take its turn")
    };
    let (sources, prepared) = member.prepare_take(&turn).await.unwrap();
    let order: Vec<u8> = sources.iter().map(|source| source.candidate.0[0]).collect();
    assert_eq!(order, [0x0b, 0x0a], "the newest head's operator, then the turn's source; never itself");
    assert_eq!(prepared.generation, SKIPPED, "the keys are sealed in the skip's generation");
    assert_eq!(prepared.bound.binding, Some(assigned.binding));
    // A latest assignment naming another holding than the turn the member reads refuses the take.
    assignments.set(Some(Assigned { reason: Reason::Skip, ..skip_of_own([0x0e; 32], 0x0e) }));
    assert!(member.prepare_take(&turn).await.is_err());
}

#[tokio::test]
async fn after_a_failed_stint_a_member_follows_the_operator_whose_head_is_newest() {
    let fixture = Fixture::start("member-skip-follow", true).await;
    fixture.registry.name("named.test");
    fixture.time.set(OWN + 1);
    let assignments = Arc::new(FakeAssignments::default());
    assignments.set(Some(Assigned { head_release: Some([0x0a; 32]), ..skip_of_own([0x0e; 32], 0x0e) }));
    *assignments.members.lock().unwrap() = vec![Registered { source: operator_of(0x0a), held: Some(0) }];
    let mut member = fixture.member_reading(&assignments);
    let turn = fixture.registry.turns()[2].clone();
    assert_eq!(member.resolve_following(Some(&turn)).await.unwrap().origin, "https://named.test");
    member.fallback.follow_holder = true;
    let holder = member.resolve_following(Some(&turn)).await.unwrap();
    assert_eq!(holder.origin, "https://10.test", "the operator whose release signed the newest head");
    // With no head, or no operator running its release, it follows the one the registry names.
    assignments.set(Some(Assigned { head_release: Some([0x99; 32]), ..skip_of_own([0x0e; 32], 0x0e) }));
    assert_eq!(member.resolve_following(Some(&turn)).await.unwrap().origin, "https://named.test");
}

/// After a reorg removes a skip, the latest block names the drawn member again: one that handed its keys
/// to the skip's member and holds no sealed copy any more takes them back, asking first the operator whose
/// head is newest, the skip's orphaned member, which still holds them and sequences.
#[tokio::test]
async fn after_a_reorg_removes_a_skip_the_drawn_member_takes_the_keys_from_the_orphaned_recipient() {
    let fixture = Fixture::start("member-skip-reorged", true).await;
    fixture.registry.draw(fixture.release().id());
    fixture.registry.source(operator_of(0x0a));
    fixture.time.set(OWN + 1);
    let assignments = Arc::new(FakeAssignments::default());
    let drawn = Assigned {
        reason: Reason::Draw,
        head_release: Some([0x0b; 32]),
        ..skip(12, 0, 0x0e, fixture.release().id(), GENERATION)
    };
    assignments.set(Some(drawn));
    *assignments.members.lock().unwrap() = vec![
        Registered { source: operator_of(0x0b), held: None },
        Registered { source: operator_of(0x0a), held: None },
    ];
    let mut member = fixture.member_reading(&assignments);
    let Holding::Take(turn) = member.holding().await.unwrap() else { panic!("the drawn member did not take its turn") };
    assert_eq!((turn.generation, turn.release), (GENERATION, fixture.release().id()));
    let (sources, prepared) = member.prepare_take(&turn).await.unwrap();
    let order: Vec<u8> = sources.iter().map(|source| source.candidate.0[0]).collect();
    assert_eq!(order, [0x0b, 0x0a], "the orphaned recipient, whose head is newest, then the turn's source");
    assert_eq!(prepared.bound.binding, Some(drawn.binding), "the take binds the draw");
}

/// A drawn member that took its turn late and handed it to a skip's member takes it back, in the same
/// process, once a reorg removes the skip; one that handed its keys on with the draw still current
/// follows.
#[tokio::test]
async fn a_member_that_handed_its_drawn_turn_to_a_skip_takes_it_back_once_a_reorg_removes_the_skip() {
    let fixture = Fixture::start("member-skip-restored", true).await;
    fixture.registry.draw(fixture.release().id());
    fixture.time.set(OWN + 1);
    let drawn = Assigned { reason: Reason::Draw, ..skip(12, 0, 0x0e, fixture.release().id(), GENERATION) };
    let assignments = Arc::new(FakeAssignments::default());
    assignments.set(Some(skip(12, 1, 0x0b, [0x0b; 32], SKIPPED)));
    let mut member = fixture.member_reading(&assignments);
    member.handed_over(Handed { generation: Some(GENERATION), reached: None, head: None });
    assert!(matches!(member.holding().await.unwrap(), Holding::Follow(_)), "it took back what it handed on");
    assignments.set(Some(drawn));
    match member.holding().await.unwrap() {
        Holding::Take(turn) => assert_eq!((turn.turn, turn.generation), (12, GENERATION)),
        _ => panic!("the member did not take back its drawn turn"),
    }
    let mut member = fixture.member_reading(&assignments);
    member.handed_over(Handed { generation: Some(GENERATION), reached: None, head: None });
    assert!(matches!(member.holding().await.unwrap(), Holding::Follow(_)), "it took back a draw it handed on");
}
