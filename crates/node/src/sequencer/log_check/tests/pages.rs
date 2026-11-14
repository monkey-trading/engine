//! The pages a held check raises: no member can be asked for it, or this node can take no part in it;
//! the run loop's withdrawal of a check rotation ends; and an upgrade that keeps asking for the check
//! its member's earlier release left due.

use super::*;

use tokio::sync::oneshot;

use crate::sequencer::Request;
use crate::test_support::{Alert, capture_alerts_and_resolutions, wait_for};

#[test]
fn a_check_no_member_can_be_asked_for_pages_until_it_can() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let mut node = Node::start("log-check-unattributable", &[other()]);
    node.approve(1);
    let (turn, taken_seq) = node.take_turn();
    // A verified head before records that start at the `TurnTaken`, as a start from a snapshot past
    // that head gives: they cannot say who sequenced the commands between the two.
    node.seq.log_check.verified = Some(head_at(taken_seq - 3));
    let mut partial = crate::log_check::Takings::default();
    let taken = Event::TurnTaken { turn, candidate: CANDIDATE, release: node.release };
    partial.command(taken_seq, &[taken]);
    node.seq.log_check.takings = partial;
    node.seq.refresh_log_check();
    assert!(node.due.borrow().is_none(), "a check came due over a range no record attributes");
    let paged = named(&mut alerts, "alert", "log_check_unattributable");
    assert_eq!(paged.len(), 1, "{paged:?}");
    assert_eq!(paged[0]["turn"], turn.to_string());
    node.seq.refresh_log_check();
    assert!(named(&mut alerts, "alert", "log_check_unattributable").is_empty(), "the alert paged again");
    // With no verified head, the range is the holding the records start at: the check comes due and
    // the episode ends.
    node.seq.log_check.verified = None;
    node.seq.refresh_log_check();
    assert!(node.due.borrow().is_some(), "the check did not come due");
    let ended = named(&mut resolved, "alert_resolved", "log_check_unattributable");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0]["turn"], turn.to_string());
}

#[test]
fn a_node_that_takes_no_turns_pages_while_the_gate_holds_its_payouts() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let mut node = Node::start("log-check-unavailable", &[other()]);
    node.approve(1);
    node.take_turn();
    // As a node without `operator.rotation_enabled` and sealed keys recovers it.
    node.seq.log_check = LogCheck::default();
    node.seq.refresh_log_check();
    assert_eq!(named(&mut alerts, "alert", "log_check_unavailable").len(), 1);
    node.seq.refresh_log_check();
    assert!(named(&mut alerts, "alert", "log_check_unavailable").is_empty(), "the alert paged again");
    node.release();
    assert!(node.signed.try_recv().is_err(), "an approval no check covers was signed");
    // Rotation off lifts the gate and ends the episode.
    let off =
        Command::SwitchRotation { enabled: false, election_cancelled: false, block: 14, chain_ref: "0x04:0".into() };
    node.seq.sequence(off).unwrap().expect("admitted");
    node.seq.refresh_log_check();
    assert_eq!(named(&mut resolved, "alert_resolved", "log_check_unavailable").len(), 1);
    node.release();
    assert_eq!(node.signed.try_recv().expect("#14's gate alone signs it").nonce, 1);
}

#[tokio::test]
async fn the_run_loop_withdraws_a_check_once_rotation_turns_off() {
    let mut node = Node::start("log-check-run-loop", &[other()]);
    node.approve(1);
    node.take_turn();
    let mut due = node.due.clone();
    assert!(due.borrow_and_update().is_some(), "no check came due");
    let Node { seq, signed: _signed, _queued: _queued_work, .. } = node;
    let (tx, rx) = mpsc::channel(4);
    let run = tokio::spawn(seq.run(rx));
    let off =
        Command::SwitchRotation { enabled: false, election_cancelled: false, block: 14, chain_ref: "0x04:0".into() };
    let (reply, events) = oneshot::channel();
    tx.send(Request::Submit { cmd: off, reply: Some(reply) }).await.unwrap();
    let events = events.await.unwrap();
    assert!(!events.iter().any(|event| matches!(event, Event::ElectionRejected { .. })), "{events:?}");
    // No checkpoint follows: the batch itself withdraws the check.
    wait_for("the asker to see the check withdrawn", async {
        while due.borrow_and_update().is_some() {
            due.changed().await.unwrap();
        }
    })
    .await;
    drop(tx);
    run.await.unwrap().unwrap();
}

#[test]
fn an_upgrade_while_a_check_is_due_still_asks_for_it() {
    let mut node = Node::start("log-check-upgrade", &[other()]);
    node.approve(1);
    let (turn, _) = node.take_turn();
    let before = node.due.borrow().clone().expect("no check came due");
    // The member's next release starts with the keys, and logs no `TurnTaken` of its own.
    let Node { seq, clock, dir, member, store, release, .. } = node;
    drop(seq);
    let sealer = MockSealer { launch_digest: [0x45; 48] };
    let keys = scratch_dir("log-check-upgrade-next-keys");
    let upgraded = Arc::new(KeyStore::create(&keys, &sealer, CONFIG, 1, store.keys()).unwrap());
    let node = Node::recover(dir, clock, (turn, member), upgraded);
    assert_ne!(node.release, release, "the upgrade runs another release");
    let due = node.due.borrow().clone().expect("the upgraded release asks for no check");
    assert_eq!((due.turn, due.excluded), (turn, before.excluded), "the member and release that sequenced it");
}

// ── Helpers ──

/// The alerts, or their ends, whose `key` field names `name`, that `lines` has received.
fn named(lines: &mut mpsc::UnboundedReceiver<Alert>, key: &str, name: &str) -> Vec<Alert> {
    std::iter::from_fn(|| lines.try_recv().ok()).filter(|alert| alert.get(key).is_some_and(|got| got == name)).collect()
}
