//! A member's alert episodes while it follows: each pages once, and its resolution repeats the fields
//! it paged with, so the router ends the episode it opened.

use super::*;
use crate::test_support::{Alert, capture_alerts_and_resolutions};

/// A failing stint pages `standby_follow_failed` once, naming the operator; once the member follows
/// another operator, the resolution names the one the page did.
#[tokio::test]
async fn a_follow_failure_pages_once_and_resolves_under_the_operator_it_named() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let fixture = Fixture::start("member-follow-resolves", true).await;
    let operator = fixture.operator(true).await;
    fixture.registry.name("127.0.0.1:9");
    fixture.time.set(SECOND + COMING_UP + 1);
    let mut member = fixture.member();
    let drive = async {
        let paged = next(&mut alerts, "standby_follow_failed").await;
        assert_eq!(paged["operator"], "https://127.0.0.1:9");
        fixture.registry.name(&operator.host);
        let ended = next(&mut resolved, "standby_follow_failed").await;
        assert_eq!(ended["operator"], paged["operator"], "the resolution names another episode");
        let pages = std::iter::from_fn(|| alerts.try_recv().ok());
        assert_eq!(pages.filter(|alert| alert["alert"] == "standby_follow_failed").count(), 0, "paged twice");
    };
    tokio::select! {
        _ = member.until_turn() => panic!("the member stopped following"),
        () = drive => {}
    }
}

/// A copy that does not open is retried each poll and pages `member_follow_blocked` once it has
/// failed `PERSISTS` polls in a row, once; the episode ends once the copy opens.
#[tokio::test]
async fn a_copy_that_does_not_open_pages_once_it_persists_and_resolves_once_it_opens() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let fixture = Fixture::start("member-open-blocked", true).await;
    let operator = fixture.operator(true).await;
    fixture.registry.name(&operator.host);
    fixture.time.set(FIRST + 1);
    let manifest = fixture.cfg.node.log_dir.join(MANIFEST);
    std::fs::write(&manifest, "not a manifest").unwrap();
    let mut member = fixture.member();
    let drive = async {
        let paged = next(&mut alerts, "member_follow_blocked").await;
        assert_eq!((paged["step"].as_str(), paged["level"].as_str()), ("open_copy", "ERROR"));
        std::fs::copy(fixture.log.join(MANIFEST), &manifest).unwrap();
        let ended = next(&mut resolved, "member_follow_blocked").await;
        assert_eq!(ended["step"], "open_copy");
        let pages = std::iter::from_fn(|| alerts.try_recv().ok());
        assert_eq!(pages.filter(|alert| alert["alert"] == "member_follow_blocked").count(), 0, "paged twice");
    };
    tokio::select! {
        _ = member.until_turn() => panic!("the member stopped following"),
        () = drive => {}
    }
}

/// A posted head the copy contradicts pages `operator_log_rollback` once however often it is read,
/// and a later posted head the copy holds ends the episode, naming the head it paged.
#[tokio::test]
async fn a_contradicting_posted_head_pages_once_and_resolves_on_a_held_one() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let fixture = Fixture::start("member-posted-resolves", true).await;
    let operator = fixture.operator(true).await;
    fixture.registry.name(&operator.host);
    fixture.time.set(FIRST + 1);
    let held = crate::sequencer::replay_path(&fixture.log).unwrap().prefix_ring;
    let (seq, later) = (3, 5);
    *fixture.heads.0.lock().unwrap() = Some(Head { seq, link: B256::repeat_byte(0x99) });
    let mut member = fixture.member();
    let drive = async {
        let paged = next(&mut alerts, "operator_log_rollback").await;
        assert_eq!(paged["seq"], seq.to_string());
        operator.wait_for_connections(operator.connections() + 2).await;
        *fixture.heads.0.lock().unwrap() = Some(Head { seq: later, link: held.prefix_at(later).unwrap() });
        let ended = next(&mut resolved, "operator_log_rollback").await;
        assert_eq!((&ended["seq"], &ended["posted_link"]), (&paged["seq"], &paged["posted_link"]));
        let pages = std::iter::from_fn(|| alerts.try_recv().ok());
        assert_eq!(pages.filter(|alert| alert["alert"] == "operator_log_rollback").count(), 0, "paged twice");
    };
    tokio::select! {
        _ = member.until_turn() => panic!("the member stopped following"),
        () = drive => {}
    }
}

/// The next line on `lines` naming `alert`, as a page's `alert` or a resolution's `alert_resolved`.
async fn next(lines: &mut tokio::sync::mpsc::UnboundedReceiver<Alert>, alert: &str) -> Alert {
    crate::test_support::wait_for(alert, async {
        loop {
            match lines.recv().await {
                Some(line) if line.get("alert").or(line.get("alert_resolved")).is_some_and(|name| name == alert) => {
                    break line;
                }
                Some(_) => {}
                None => panic!("the alert channel closed"),
            }
        }
    })
    .await
}
