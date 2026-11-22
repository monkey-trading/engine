//! The holders the sealed keys name (#596): a handover names this release through its final checkpoint
//! while the gate applies, and none while it does not; a restart restores them; the check due excludes
//! each one past the verified head, so its member's or release's verification is refused; a verification
//! that reaches its last command drops it from the keys, and once the gate stops applying every one goes,
//! with a verified head or without; with nothing verified, one whose commands precede the range is
//! outside it.

use super::*;

use crate::log_check::Holder;

#[test]
fn a_handover_names_this_release_a_holder_through_its_final_checkpoint_only_while_the_gate_applies() {
    let mut node = Node::start("log-check-holder-named", &[other()]);
    node.approve(1);
    node.take_turn();
    node.seq.checkpoint().unwrap();
    let through = node.seq.engine.last_seq().unwrap();
    assert!(handed_over(&mut node), "the handover's stop was refused");
    let named = Holder { through, release: node.release, candidates: [CANDIDATE].into() };
    assert_eq!(node.store.keys().holders, std::slice::from_ref(&named), "the keys the handover sends do not name it");
    assert_eq!(node.seq.log_check.holders, [named], "the sequencer does not exclude it");

    let mut node = Node::start("log-check-holder-ungated", &[other()]);
    node.approve(1);
    node.take_turn();
    node.end_turns();
    node.seq.checkpoint().unwrap();
    assert!(handed_over(&mut node), "the handover's stop was refused");
    assert!(node.store.keys().holders.is_empty(), "keys handed over once the turns ended name a holder");
}

/// A holder record that cannot be sealed refuses the handover's stop, which the predecessor reports
/// while it goes on serving, and this node goes on sequencing; a stop once the keys can be sealed names
/// it.
#[test]
fn a_holder_record_that_cannot_be_sealed_refuses_the_stop_and_sequencing_goes_on() {
    let name = "log-check-holder-unsealed";
    let mut node = Node::start(name, &[other()]);
    node.approve(1);
    node.take_turn();
    node.seq.checkpoint().unwrap();
    let blocked = unsealable(name, &node.store);
    let (reply, stopped) = tokio::sync::oneshot::channel();
    let committed = node.seq.seal_handover_tail(Some(reply));
    let refused = stopped.blocking_recv().unwrap().err();
    std::fs::remove_dir(blocked).unwrap();
    assert!(!committed, "the stop committed with no holder record sealed");
    let refused = refused.expect("the stop went through with no holder record sealed");
    assert!(refused.downcast_ref::<crate::sequencer::HandoverRefused>().is_some(), "not a refusal: {refused:#}");
    assert!(node.store.keys().holders.is_empty() && node.seq.log_check.holders.is_empty());
    node.seq.sequence(Command::Tick).unwrap().expect("sequencing stopped");
    node.seq.checkpoint().unwrap();
    assert!(handed_over(&mut node), "the stop once the keys could be sealed was refused");
    assert_eq!(node.store.keys().holders.len(), 1, "the stop once the keys could be sealed named no holder");
}

#[test]
fn a_holder_past_the_verified_head_is_excluded_until_a_verification_reaches_its_last_command() {
    let mut node = Node::start("log-check-holder-excluded", &[other(), third()]);
    node.approve(1);
    node.take_turn();
    let head = node.due.borrow().clone().expect("no check came due").head;
    // The other member handed the keys on, its last command at the check's head, as a reorged skip's does.
    let orphaned = Holder { through: head.seq, release: OTHER, candidates: [other().candidate].into() };
    node.store.update(|keys| keys.holders.push(orphaned.clone())).unwrap();
    let mut node = node.restart();
    let due = node.due.borrow().clone().expect("no check came due after the restart");
    assert!(due.excluded.include(other().candidate, THIRD) && due.excluded.include(third().candidate, OTHER));
    assert!(!node.seq.log_checked(head, (other().candidate, THIRD)), "the holder's member verified its commands");
    assert!(!node.seq.log_checked(head, (third().candidate, OTHER)), "the holder's release verified its commands");
    assert_eq!(node.store.keys().holders, [orphaned], "a refused verification dropped the holder");
    // A third member's verification reaches the holder's last command, which drops it.
    assert!(node.seq.log_checked(head, (third().candidate, THIRD)));
    assert!(node.store.keys().holders.is_empty() && node.seq.log_check.holders.is_empty(), "the holder stayed");
}

/// With nothing verified, the check's range is this node's holding from its `TurnTaken`: a holder whose
/// commands all precede it, as the operator in office before the first turn names itself at its stop,
/// is outside it, so in a two-member set its member is asked and its verification counts.
#[test]
fn with_nothing_verified_a_holder_before_the_turn_taken_is_outside_the_check() {
    let mut node = Node::start("log-check-holder-before", &[other()]);
    node.approve(1);
    let (_, taken_seq) = node.take_turn();
    let head = node.due.borrow().clone().expect("no check came due").head;
    let before = Holder { through: taken_seq - 1, release: OTHER, candidates: [other().candidate].into() };
    node.store.update(|keys| keys.holders.push(before)).unwrap();
    let mut node = node.restart();
    assert!(node.seq.log_check.verified.is_none(), "a head was verified before the first check");
    let due = node.due.borrow().clone().expect("no check came due after the restart");
    assert!(!due.excluded.include(other().candidate, OTHER), "a holder before the range excluded its member");
    assert!(node.seq.log_checked(head, (other().candidate, OTHER)), "the other member's verification was refused");
}

#[test]
fn every_holder_goes_once_the_gate_stops_applying_with_a_verified_head_or_without() {
    for verified in [false, true] {
        let mut node = Node::start(&format!("log-check-holder-dropped-{verified}"), &[other(), third()]);
        node.approve(1);
        node.take_turn();
        let head = node.due.borrow().clone().expect("no check came due").head;
        if verified {
            assert!(node.seq.log_checked(head, (third().candidate, THIRD)));
        }
        let later = Holder { through: head.seq + 1, release: OTHER, candidates: [other().candidate].into() };
        node.store.update(|keys| keys.holders.push(later)).unwrap();
        let mut node = node.restart();
        node.end_turns();
        node.seq.refresh_log_check();
        let keys = node.store.keys();
        assert!(
            keys.holders.is_empty() && keys.log_check.is_none(),
            "verified {verified}: the keys kept the gate's records"
        );
    }
}

/// With the gate off, a handover's stop sends no keys carrying the gate's records: records a refresh
/// could not drop refuse the stop until they are dropped with it.
#[test]
fn a_stop_with_the_gate_off_drops_the_gates_records_or_is_refused() {
    let name = "log-check-holder-ungated-stop";
    let mut node = Node::start(name, &[other()]);
    node.approve(1);
    node.take_turn();
    let later = Holder { through: u64::MAX, release: OTHER, candidates: [other().candidate].into() };
    node.store.update(|keys| keys.holders.push(later)).unwrap();
    let mut node = node.restart();
    let blocked = unsealable(name, &node.store);
    node.end_turns();
    node.seq.refresh_log_check();
    node.seq.checkpoint().unwrap();
    let (reply, stopped) = tokio::sync::oneshot::channel();
    let committed = node.seq.seal_handover_tail(Some(reply));
    let refused = stopped.blocking_recv().unwrap().err();
    std::fs::remove_dir(blocked).unwrap();
    assert!(!committed, "the stop committed with the keys carrying the gate's records");
    let refused = refused.expect("the stop went through with the keys carrying the gate's records");
    assert!(refused.downcast_ref::<crate::sequencer::HandoverRefused>().is_some(), "not a refusal: {refused:#}");
    assert!(!node.store.keys().holders.is_empty(), "the records were dropped though the keys could not be sealed");
    node.seq.sequence(Command::Tick).unwrap().expect("sequencing stopped");
    node.seq.checkpoint().unwrap();
    assert!(handed_over(&mut node), "the stop once the keys could be sealed was refused");
    assert!(node.store.keys().holders.is_empty(), "keys handed over with the gate off carry holders");
}

// ── Helpers ──

/// A third member's release.
const THIRD: B256 = B256::repeat_byte(0xb3);

/// A third member of the set.
fn third() -> SetMember {
    SetMember { candidate: AccountId([0xc3; 20]), admin_key: AccountId([0xd3; 20]), weight: TokenUnits(1) }
}

/// Makes every seal over `store`'s current file (`update`), the keys of the node `name` starts, fail
/// until the returned directory is removed: it stands where that seal writes its partial file, which no
/// one, root included, can open as a file. A seal into another generation is not blocked.
fn unsealable(name: &str, store: &crate::tee::sealing::KeyStore) -> std::path::PathBuf {
    let dir = crate::test_support::scratch_path(&format!("{name}-keys"));
    let sealed = crate::tee::sealing::file_for(&dir, &store.release(), store.generation());
    assert!(sealed.exists(), "no sealed keys at {}", sealed.display());
    let partial = sealed.with_extension("partial");
    std::fs::create_dir(&partial).unwrap();
    partial
}

/// Stops `node` for a handover, as the node's drain does once the checkpoint gate admits it: whether the
/// stop went through.
fn handed_over(node: &mut Node) -> bool {
    let (reply, stopped) = tokio::sync::oneshot::channel();
    assert!(node.seq.seal_handover_tail(Some(reply)), "the final segment was not sealed");
    stopped.blocking_recv().unwrap().is_ok()
}
