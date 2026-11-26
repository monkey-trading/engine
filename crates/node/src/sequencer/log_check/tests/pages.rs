//! The pages a held check raises: no member can be asked for it, this node can take no part in it, or
//! approvals wait for the installation after the last drawn turn, which this node does not hold; the
//! holder of that turn checking approvals within it; the run loop's withdrawal of a check the turns'
//! end lifts; and an upgrade that keeps asking for the check its member's earlier release left due.

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
    // The turns' end lifts the gate and ends the episode.
    node.end_turns();
    node.seq.refresh_log_check();
    assert_eq!(named(&mut resolved, "alert_resolved", "log_check_unavailable").len(), 1);
    node.release();
    assert_eq!(node.signed.try_recv().expect("#14's gate alone signs it").nonce, 1);
}

/// The holder of the last drawn turn, with rotation off and no later turn assigned, still asks for the
/// check of each checkpoint that covers a waiting approval: no installation pages, and a member's check
/// releases the approval.
#[test]
fn the_holder_of_the_last_drawn_turn_checks_approvals_within_it_after_rotation_is_turned_off() {
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    let mut node = Node::start("log-check-last-drawn-holder", &[other()]);
    node.approve(1);
    node.take_turn();
    let head = node.due.borrow().clone().expect("no check came due").head;
    assert!(node.seq.log_checked(head, (other().candidate, OTHER)));
    node.turn_rotation_off();
    node.approve(2);
    let due = node.due.borrow().clone().expect("no check came due for the approval in the last drawn turn");
    assert_eq!(node.check_of(2), Some(CheckWait::Check { checkpoint_seq: Some(due.head.seq) }));
    assert!(named(&mut alerts, "alert", "log_check_awaiting_installation").is_empty(), "the holder paged");
    assert!(node.seq.log_checked(due.head, (other().candidate, OTHER)));
    node.release();
    let signed: Vec<u64> =
        std::iter::from_fn(|| node.signed.try_recv().ok()).map(|withdrawal| withdrawal.nonce).collect();
    assert_eq!(signed, [1, 2], "the check in the last drawn turn released nothing");
}

/// While another member's node holds the last drawn turn, with rotation off and no later turn assigned,
/// this node asks for no check, so an approval waits for the installation that ends the turns: the
/// payouts read says so, and a Warning pages once, until the installation lifts the gate.
#[test]
fn approvals_wait_for_the_installation_and_page_while_another_member_holds_the_last_drawn_turn() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let mut node = Node::start("log-check-installation", &[other()]);
    node.approve(1);
    node.take_turn();
    let head = node.due.borrow().clone().expect("no check came due").head;
    assert!(node.seq.log_checked(head, (other().candidate, OTHER)));
    node.approve(2);
    let turn = other_takes_turn(&mut node);
    node.seq.refresh_log_check();
    assert!(named(&mut alerts, "alert", "log_check_awaiting_installation").is_empty(), "paged with rotation on");
    node.turn_rotation_off();
    node.seq.refresh_log_check();
    assert_eq!(node.check_of(2), Some(CheckWait::Installation { turn }), "the read names a boundary that never comes");
    let paged = named(&mut alerts, "alert", "log_check_awaiting_installation");
    assert_eq!(paged.len(), 1, "{paged:?}");
    assert_eq!(paged[0]["turn"], turn.to_string());
    node.seq.refresh_log_check();
    assert!(named(&mut alerts, "alert", "log_check_awaiting_installation").is_empty(), "the alert paged again");
    node.end_turns();
    node.seq.refresh_log_check();
    assert_eq!(node.check_of(2), None, "the installation left the approval waiting");
    let ended = named(&mut resolved, "alert_resolved", "log_check_awaiting_installation");
    assert_eq!(ended.len(), 1, "{ended:?}");
}

/// The approvals waiting for the installation are counted at most once a minute of sequenced time, so
/// the batches between pay for no scan of them.
#[test]
fn waiting_approvals_are_counted_at_most_once_a_minute() {
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    let mut node = Node::start("log-check-installation-count", &[other()]);
    node.approve(1);
    node.take_turn();
    let head = node.due.borrow().clone().expect("no check came due").head;
    assert!(node.seq.log_checked(head, (other().candidate, OTHER)));
    node.approve(2);
    other_takes_turn(&mut node);
    node.turn_rotation_off();
    node.seq.refresh_log_check();
    assert_eq!(named(&mut alerts, "alert", "log_check_awaiting_installation").len(), 1, "no page");
    // Forgotten as if never raised, the alert pages again only at the next count.
    node.seq.log_check.awaiting_installation = None;
    node.seq.refresh_log_check();
    assert!(
        named(&mut alerts, "alert", "log_check_awaiting_installation").is_empty(),
        "counted again within the minute"
    );
    node.clock.advance(Duration::from_secs(61));
    node.seq.trust(sample_around(node.clock.as_ref())).unwrap();
    node.seq.sequence(Command::Tick).unwrap().expect("admitted");
    node.seq.refresh_log_check();
    assert_eq!(named(&mut alerts, "alert", "log_check_awaiting_installation").len(), 1, "not counted a minute on");
}

/// While another member's node holds the turn in effect, a turn drawn after it keeps its boundary in the
/// payouts read after rotation is turned off, and nothing pages.
#[test]
fn a_later_drawn_turn_keeps_its_boundary_after_rotation_is_turned_off() {
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    let mut node = Node::start("log-check-later-drawn", &[other()]);
    node.approve(1);
    node.take_turn();
    let head = node.due.borrow().clone().expect("no check came due").head;
    assert!(node.seq.log_checked(head, (other().candidate, OTHER)));
    node.approve(2);
    let turn = other_takes_turn(&mut node);
    let (candidate, admin_key, next) = (other().candidate, other().admin_key, turn + 1);
    let (reason, block, chain_ref) = (TurnReason::Draw, 17, "0x07:0".to_owned());
    let assignment = RegistryTurn { turn: next, set: 1, position: 0, reason, candidate, admin_key, block, chain_ref };
    let events = node.seq.sequence(Command::AssignTurn(assignment)).unwrap().expect("admitted");
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
    node.turn_rotation_off();
    node.seq.refresh_log_check();
    let boundary_ms = engine_core::turns::boundary(next).unwrap();
    assert_eq!(node.check_of(2), Some(CheckWait::Boundary { turn: next, boundary_ms }));
    assert!(named(&mut alerts, "alert", "log_check_awaiting_installation").is_empty(), "paged with a later turn drawn");
}

#[tokio::test]
async fn the_run_loop_withdraws_a_check_once_the_turns_end() {
    let mut node = Node::start("log-check-run-loop", &[other()]);
    node.approve(1);
    node.take_turn();
    node.turn_rotation_off();
    node.seq.refresh_log_check();
    let mut due = node.due.clone();
    assert!(due.borrow_and_update().is_some(), "no check came due, or it was withdrawn with rotation off");
    let Node { seq, signed: _signed, _queued: _queued_work, .. } = node;
    let (tx, rx) = mpsc::channel(4);
    let run = tokio::spawn(seq.run(rx));
    let installation = engine_core::election::RegistryInstallation {
        candidate: AccountId([0xc9; 20]),
        admin_key: AccountId([0xd9; 20]),
        block: 16,
        chain_ref: "0x06:0".into(),
    };
    let (reply, events) = oneshot::channel();
    tx.send(Request::Submit { cmd: Command::InstallOperator(installation), reply: Some(reply) }).await.unwrap();
    let events = events.await.unwrap();
    assert!(events.iter().any(|event| matches!(event, Event::OperatorInstalled { .. })), "{events:?}");
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

/// Moves `node` into the turn after its own, the other member's, and sequences that member's `TurnTaken`
/// of it, as a log another node sequences records it: the turn.
fn other_takes_turn(node: &mut Node) -> u64 {
    node.hand_over_turn();
    let turn = node.turn + 1;
    let events = node.seq.sequence(Command::TurnTaken { turn, release: OTHER }).unwrap().expect("admitted");
    assert!(matches!(events.as_slice(), [Event::TurnTaken { .. }]), "{events:?}");
    turn
}

/// The alerts, or their ends, whose `key` field names `name`, that `lines` has received.
fn named(lines: &mut mpsc::UnboundedReceiver<Alert>, key: &str, name: &str) -> Vec<Alert> {
    std::iter::from_fn(|| lines.try_recv().ok()).filter(|alert| alert.get(key).is_some_and(|got| got == name)).collect()
}
