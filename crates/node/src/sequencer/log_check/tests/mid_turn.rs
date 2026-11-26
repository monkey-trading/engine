//! Checks within a turn this node holds (#1079): an approval past the verified head comes due at the
//! first checkpoint written at or after it, reusing that checkpoint, and none before; the check due keeps
//! its checkpoint while later ones are written, and the next comes due once it is verified; a quiet
//! stretch asks for none; the boundary's check comes first; and holding on through a boundary checks only
//! the range since the last check within the turn.

use super::*;

use crate::checkpoints::Registry;

#[test]
fn an_approval_within_a_held_turn_comes_due_at_the_first_checkpoint_covering_it_and_is_signed_once_verified() {
    let mut node = Node::start("log-check-within-turn", &[other()]);
    node.approve(1);
    let (turn, _) = node.take_turn();
    let boundary = node.due.borrow().clone().expect("no check came due at the boundary").head;
    assert!(node.seq.log_checked(boundary, (other().candidate, OTHER)));
    node.release();
    assert_eq!(node.signed.try_recv().expect("the boundary's check released the approval").nonce, 1);

    // Checkpoints with no approval waiting past the verified head ask for no check.
    for _ in 0..3 {
        node.seq.sequence(Command::Tick).unwrap().expect("admitted");
        node.seq.checkpoint().unwrap();
    }
    assert!(node.due.borrow().is_none(), "a checkpoint with no approval waiting asked for a check");

    // A signed checkpoint not yet final holds the approval's own back: no check comes due before one covers it.
    hold_paced_checkpoints(&mut node);
    node.approve(2);
    let approved = node.seq.engine.last_seq().unwrap();
    assert!(node.last_checkpoint().seq < approved, "the approval's checkpoint was not held back");
    node.seq.refresh_log_check();
    assert!(node.due.borrow().is_none(), "a check came due before a checkpoint covered the approval");
    assert_eq!(node.check_of(2), Some(CheckWait::Check { checkpoint_seq: None }));

    // Once the vault holds the last one, the next checkpoint covers the approval and its check comes due.
    node.seq.checkpoint_status(Some(node.last_checkpoint().seq), Some(Health::Healthy)).unwrap();
    node.seq.paced_checkpoint().unwrap();
    let due = node.due.borrow().clone().expect("no check came due at the checkpoint covering the approval");
    assert_eq!((due.turn, due.head, due.from), (turn, node.checkpoint_head(approved), boundary.seq + 1));
    assert_eq!(node.check_of(2), Some(CheckWait::Check { checkpoint_seq: Some(approved) }));
    node.release();
    assert!(node.signed.try_recv().is_err(), "an approval was signed before a member verified the log through it");

    // A later approval and its checkpoint leave the check due on its checkpoint.
    node.approve(3);
    let later = node.seq.engine.last_seq().unwrap();
    assert_eq!(node.last_checkpoint().seq, later, "the later approval's checkpoint was not written");
    assert_eq!(node.due.borrow().as_ref().map(|due| due.head), Some(due.head), "the check due moved");
    assert_eq!(node.check_of(3), Some(CheckWait::Check { checkpoint_seq: None }));

    // Its verification is sealed and releases the approval, and the later approval's check comes due at
    // once, at the latest checkpoint, over the range since.
    assert!(node.seq.log_checked(due.head, (other().candidate, OTHER)));
    assert_eq!(node.store.keys().log_check, Some(due.head), "the check within the turn was not sealed");
    let next = node.due.borrow().clone().expect("no check came due for the later approval");
    assert_eq!((next.head, next.from), (node.checkpoint_head(later), approved + 1));
    assert_eq!(node.check_of(3), Some(CheckWait::Check { checkpoint_seq: Some(later) }));
    node.release();
    assert_eq!(signed(&mut node), [2]);
    assert!(node.seq.log_checked(next.head, (other().candidate, OTHER)));
    assert!(node.due.borrow().is_none(), "a check stayed due with no approval waiting");
    node.release();
    assert_eq!(signed(&mut node), [3]);
}

#[test]
fn the_boundarys_check_comes_first_and_an_approval_past_its_checkpoint_waits_for_the_next() {
    let mut node = Node::start("log-check-boundary-first", &[other()]);
    node.approve(1);
    let (turn, _) = node.take_turn();
    let boundary = node.due.borrow().clone().expect("no check came due at the boundary");
    node.approve(2);
    let approved = node.seq.engine.last_seq().unwrap();
    assert!(boundary.head.seq < approved, "the approval is not past the boundary's checkpoint");
    assert_eq!(node.due.borrow().as_ref(), Some(&boundary), "an approval moved the boundary's check");
    assert_eq!(node.check_of(1), Some(CheckWait::Check { checkpoint_seq: Some(boundary.head.seq) }));
    assert_eq!(node.check_of(2), Some(CheckWait::Check { checkpoint_seq: None }));

    assert!(node.seq.log_checked(boundary.head, (other().candidate, OTHER)));
    let next = node.due.borrow().clone().expect("no check came due for the approval past the boundary's checkpoint");
    assert_eq!((next.turn, next.head.seq, next.from), (turn, approved, boundary.head.seq + 1));
    node.release();
    assert_eq!(signed(&mut node), [1]);
    assert!(node.seq.log_checked(next.head, (other().candidate, OTHER)));
    node.release();
    assert_eq!(signed(&mut node), [2]);
}

#[test]
fn holding_on_through_a_boundary_checks_only_the_range_since_the_last_check_within_the_turn() {
    let mut node = Node::start("log-check-hold-on", &[other()]);
    node.approve(1);
    let (turn, _) = node.take_turn();
    let boundary = node.due.borrow().clone().expect("no check came due at the boundary").head;
    assert!(node.seq.log_checked(boundary, (other().candidate, OTHER)));
    node.approve(2);
    let within = node.due.borrow().clone().expect("no check came due within the turn").head;
    assert!(node.seq.log_checked(within, (other().candidate, OTHER)));

    // The member is drawn again and holds the turn on: the boundary's check covers the commands since the
    // check within the turn, which only this member sequenced.
    let taken_seq = hold_on(&mut node);
    let held = node.due.borrow().clone().expect("no check came due at the boundary held on through");
    assert_eq!((held.turn, held.from), (turn + 1, within.seq + 1));
    assert!(held.head.seq >= taken_seq, "the check's checkpoint {} precedes the TurnTaken", held.head.seq);
    let releases = BTreeSet::from([node.release]);
    let excluded = Sequencers { candidates: BTreeSet::from([CANDIDATE]), releases, in_office: (turn, CANDIDATE) };
    assert_eq!(held.excluded, excluded);
}

// ── Helpers ──

/// Gives `node`'s gate a checkpoint registry and a signed checkpoint the vault has not finalized, which
/// holds the paced checkpoints after it back.
fn hold_paced_checkpoints(node: &mut Node) {
    node.seq.gate.as_mut().expect("the checkpoint gate").registry = Some(Registry::default());
    node.seq.sequence(Command::Tick).unwrap().expect("admitted");
    node.seq.checkpoint().unwrap();
}

/// The nonces `node` has signed since the last call.
fn signed(node: &mut Node) -> Vec<u64> {
    std::iter::from_fn(|| node.signed.try_recv().ok()).map(|withdrawal| withdrawal.nonce).collect()
}

/// Mirrors the turn after `node`'s for its own member, and moves past its boundary, where the node takes
/// it again: the seq of its `TurnTaken`.
fn hold_on(node: &mut Node) -> u64 {
    let next = node.turn + 1;
    let (reason, block, chain_ref) = (TurnReason::Draw, 15, "0x05:0".to_owned());
    let (candidate, admin_key) = (CANDIDATE, node.member);
    let assignment = RegistryTurn { turn: next, set: 1, position: 0, reason, candidate, admin_key, block, chain_ref };
    let events = node.seq.sequence(Command::AssignTurn(assignment)).unwrap().expect("admitted");
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
    let last = node.last_checkpoint();
    node.seq.checkpoint_status(Some(last.seq), Some(Health::Healthy)).unwrap();
    let begins = engine_core::turns::boundary(next).unwrap();
    node.clock.advance(Duration::from_millis((begins + 1_000 - node.clock.now_ms()) as u64));
    node.seq.trust(sample_around(node.clock.as_ref())).unwrap();
    node.seq.sequence(Command::Tick).unwrap().expect("admitted");
    let taken = node.seq.engine.daily_turns().taken.expect("the turn was taken");
    assert_eq!((taken.turn, taken.release), (next, node.release), "the member did not take the next turn");
    node.seq.log_check.takings.last().expect("the TurnTaken was recorded").0
}
