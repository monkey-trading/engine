//! The `V78` status notice through the signed admin route and `engine admin`: set, replaced and cleared
//! by the operator's signature, served at `/v1/notice`, logged with its signature, and rebuilt by a replay.

use super::*;
use engine_core::notice::{MAX_NOTICE_CHARS, Notice};

const TEXT: &str = "Security maintenance: withdrawals are paused.";

/// The notice the node serves at `/v1/notice`, as JSON.
async fn served(node: &Node) -> Value {
    let (status, body) = node.get("/v1/notice").await;
    assert_eq!(status, StatusCode::OK);
    body
}

#[tokio::test]
async fn a_notice_the_operator_signs_is_served_replaced_cleared_and_replayed() {
    let node = Node::start_on("status-notice", true).await;
    assert_eq!(served(&node).await, serde_json::json!({ "notice": null }));

    let set = node.sign(0, AdminCommand::SetNotice { text: TEXT.into() }, NOW + 60_000);
    let (status, events) = node.post("/v1/admin", serde_json::to_value(&set).unwrap()).await;
    assert_eq!((status, events), (StatusCode::OK, serde_json::json!([{ "type": "admin_applied" }])));
    assert_eq!(served(&node).await, serde_json::json!({ "notice": { "text": TEXT, "set_at_ms": NOW } }));

    let replace = node.sign(1, AdminCommand::SetNotice { text: "Withdrawals have resumed.".into() }, NOW + 60_000);
    assert_eq!(node.post("/v1/admin", serde_json::to_value(&replace).unwrap()).await.0, StatusCode::OK);
    assert_eq!(served(&node).await["notice"]["text"], "Withdrawals have resumed.");

    // The log carries both with the operator's signature, and a replay rebuilds the notice in force.
    let entries = node.entries();
    let logged: Vec<_> = admin_entries(&entries).into_iter().map(|(request, _)| request).collect();
    assert_eq!(logged, [set, replace]);
    let replayed = replay(&entries).unwrap().engine;
    assert_eq!(replayed.notice(), Some(&Notice { text: "Withdrawals have resumed.".into(), set_at_ms: NOW }));

    let clear = node.sign(2, AdminCommand::ClearNotice, NOW + 60_000);
    assert_eq!(node.post("/v1/admin", serde_json::to_value(&clear).unwrap()).await.0, StatusCode::OK);
    assert_eq!(served(&node).await, serde_json::json!({ "notice": null }));
    assert_eq!(replay(&node.entries()).unwrap().engine.notice(), None);
}

#[tokio::test]
async fn a_notice_signed_by_another_key_or_refused_for_its_text_leaves_none() {
    let node = Node::start_on("status-notice-refused", true).await;
    let other = SigningKey::from_slice(&[0x77; 32]).unwrap();
    let forged = sign(&other, &node.state.operator, AdminCommand::SetNotice { text: TEXT.into() }, 0, NOW + 60_000);
    assert_eq!(node.post("/v1/admin", serde_json::to_value(&forged).unwrap()).await.0, StatusCode::UNAUTHORIZED);

    let long = "a".repeat(MAX_NOTICE_CHARS + 1);
    let refused = node.sign(0, AdminCommand::SetNotice { text: long }, NOW + 60_000);
    let (status, events) = node.post("/v1/admin", serde_json::to_value(&refused).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(events[0]["type"], "admin_rejected", "{events}");
    assert_eq!(served(&node).await, serde_json::json!({ "notice": null }));
}

#[tokio::test]
async fn engine_admin_sets_and_clears_a_notice_and_refuses_text_it_would_not_sign() {
    let node = Node::start_on("status-notice-cli", true).await;
    let url = format!("https://{}", node.serve().await);

    let sent = node.admin_at(&url, &["set-notice", TEXT]).await.unwrap();
    assert_eq!(sent.command, AdminCommand::SetNotice { text: TEXT.into() });
    assert_eq!(served(&node).await["notice"]["text"], TEXT);

    // Refused before it is signed, so nothing reaches the log.
    let logged = admin_entries(&node.entries()).len();
    for text in ["", "<b>paused</b>", "see https://example.com"] {
        let err = node.admin_at(&url, &["set-notice", text]).await.expect_err(text);
        assert!(err.to_string().contains("the notice"), "{text:?}: {err:#}");
    }
    assert_eq!(admin_entries(&node.entries()).len(), logged);

    node.admin_at(&url, &["clear-notice"]).await.unwrap();
    assert_eq!(served(&node).await, serde_json::json!({ "notice": null }));
    let err = node.admin_at(&url, &["clear-notice"]).await.expect_err("nothing to clear");
    assert!(err.to_string().contains("no notice is in force"), "{err:#}");
}
