//! A member follows the operator in effect and moves at a turn's boundary, bootstraps an empty copy
//! from the archive, pages a posted head its copy contradicts, and holds once its keys are sealed for
//! its turn.

use super::*;

/// Under the operator in effect it follows that operator's stream; a hostname the registry moves to
/// before the boundary does not move it; at the next turn's boundary it follows the new one; once its
/// own turn's keys are sealed it holds.
#[tokio::test]
async fn a_member_follows_the_operator_in_effect_and_moves_at_the_boundary() {
    let fixture = Fixture::start("member-moves", true).await;
    let (first, second) = (fixture.operator(true).await, fixture.operator(true).await);
    fixture.registry.name(&first.host);
    fixture.time.set(FIRST + 1);
    let mut member = fixture.member();
    let drive = async {
        first.wait_for_connections(1).await;
        // The registry names another operator before the boundary: the member stays on the first.
        fixture.registry.name(&second.host);
        first.wait_for_connections(first.connections() + 2).await;
        assert_eq!(second.connections(), 0, "the member moved before the boundary");
        fixture.time.set(SECOND + 1);
        second.wait_for_connections(1).await;
        fixture.seal_own_turn();
        fixture.time.set(OWN + 1);
    };
    let (turn, ()) = tokio::join!(member.until_turn(), drive);
    assert!(matches!(turn.unwrap(), Turn::Hold), "the member did not hold its turn once its keys were sealed");
}

/// An empty copy is bootstrapped from the archive before the member follows: the stand-in stream
/// serves no history from the start of the log, so the copy holds it only through the archive.
#[tokio::test]
async fn a_member_bootstraps_an_empty_copy_from_the_archive() {
    let fixture = Fixture::start("member-bootstrap", false).await;
    let operator = fixture.operator(false).await;
    fixture.registry.name(&operator.host);
    fixture.time.set(FIRST + 1);
    let mut member = fixture.member();
    let drive = async {
        operator.wait_for_connections(2).await;
        fixture.seal_own_turn();
        fixture.time.set(OWN + 1);
    };
    let (turn, ()) = tokio::join!(member.until_turn(), drive);
    assert!(matches!(turn.unwrap(), Turn::Hold));
    assert_eq!(log_entries(&fixture.cfg.node.log_dir), log_entries(&fixture.log), "the copy is not the operator's log");
}

/// A posted log head naming another history than the copy holds at its seq pages Critical
/// `operator_log_rollback`; one the copy holds pages nothing.
#[tokio::test]
async fn a_member_pages_a_posted_head_its_copy_contradicts() {
    let (_guard, mut alerts) = capture_alerts();
    let fixture = Fixture::start("member-posted", true).await;
    let operator = fixture.operator(true).await;
    fixture.registry.name(&operator.host);
    fixture.time.set(FIRST + 1);
    let held = crate::sequencer::replay_path(&fixture.log).unwrap().prefix_ring;
    let seq = 3;
    *fixture.heads.0.lock().unwrap() = Some(Head { seq, link: held.prefix_at(seq).unwrap() });
    let mut member = fixture.member();
    let drive = async {
        operator.wait_for_connections(2).await;
        assert!(
            std::iter::from_fn(|| alerts.try_recv().ok()).all(|alert| alert["alert"] != "operator_log_rollback"),
            "a head the copy holds paged"
        );
        *fixture.heads.0.lock().unwrap() = Some(Head { seq, link: B256::repeat_byte(0x99) });
        let paged = crate::test_support::wait_for("the rollback page", async {
            loop {
                match alerts.recv().await {
                    Some(alert) if alert["alert"] == "operator_log_rollback" => break alert,
                    Some(_) => {}
                    None => panic!("the alert channel closed"),
                }
            }
        })
        .await;
        assert_eq!((paged["level"].as_str(), paged["seq"].as_str()), ("ERROR", seq.to_string().as_str()));
        fixture.seal_own_turn();
        fixture.time.set(OWN + 1);
    };
    let (turn, ()) = tokio::join!(member.until_turn(), drive);
    assert!(matches!(turn.unwrap(), Turn::Hold));
}

/// At its own turn's boundary, with no copy to take a tail onto and no keys sealed for it, the member
/// starts as a node with no copy does, without following anyone.
#[tokio::test]
async fn a_member_detects_its_turn_and_with_no_copy_starts_without_following() {
    let fixture = Fixture::start("member-own-turn", false).await;
    let operator = fixture.operator(true).await;
    fixture.registry.name(&operator.host);
    fixture.registry.draw(fixture.release().id());
    fixture.time.set(OWN + 1);
    let mut member = fixture.member();
    let turn = crate::test_support::wait_for("the member to stop for its own turn", member.until_turn()).await;
    assert!(matches!(turn.unwrap(), Turn::Hold));
    assert_eq!(operator.connections(), 0, "the member followed at its own turn");
    assert!(!file_for(&fixture.keys, &fixture.release(), GENERATION).exists(), "keys were sealed");
}

/// A member that handed its turn over never holds it again: a later trusted time sample just below
/// the successor's boundary, in its own turn, whose keys it still has sealed, does not give it that
/// turn back; and the generation it gave away keeps it off those keys even with no floor.
#[tokio::test]
async fn a_member_that_handed_its_turn_over_follows_on_a_sample_below_the_boundary() {
    let fixture = Fixture::start("member-handed", true).await;
    let operator = fixture.operator(true).await;
    fixture.registry.name(&operator.host);
    fixture.seal_own_turn();
    fixture.time.set(AFTER - 5_000);
    let mut member = fixture.member();
    member.handed_over(Handed { generation: Some(GENERATION), reached: Some(AFTER + 1), head: None });
    tokio::select! {
        _ = member.until_turn() => panic!("the member came back to the turn it handed over"),
        () = operator.wait_for_connections(1) => {}
    }

    let mut member = fixture.member();
    member.handed_over(Handed { generation: Some(GENERATION), reached: None, head: None });
    let followed = operator.connections() + 1;
    tokio::select! {
        _ = member.until_turn() => panic!("the member held the generation it handed over"),
        () = operator.wait_for_connections(followed) => {}
    }
}

/// While a new turn's member takes the keys and comes up, a member following it expects failed
/// stints and pages nothing; once `COMING_UP` has passed, a failure pages `standby_follow_failed`.
#[tokio::test]
async fn a_member_pages_no_follow_failure_while_the_turns_member_comes_up() {
    let (_guard, mut alerts) = capture_alerts();
    let fixture = Fixture::start("member-coming-up", true).await;
    fixture.registry.name("127.0.0.1:9");
    fixture.time.set(SECOND + 1);
    let mut member = fixture.member();
    let drive = async {
        tokio::time::sleep(STINT * 2).await;
        let paged =
            std::iter::from_fn(|| alerts.try_recv().ok()).any(|alert| alert["alert"] == "standby_follow_failed");
        assert!(!paged, "a failure while the turn's member came up paged");
        fixture.time.set(SECOND + COMING_UP + 1);
        crate::test_support::wait_for("the follow failure's page", async {
            loop {
                match alerts.recv().await {
                    Some(alert) if alert["alert"] == "standby_follow_failed" => break,
                    Some(_) => {}
                    None => panic!("the alert channel closed"),
                }
            }
        })
        .await;
    };
    tokio::select! {
        _ = member.until_turn() => panic!("the member stopped following"),
        () = drive => {}
    }
}
