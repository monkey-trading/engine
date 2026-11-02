use engine_core::command::{CancelAllRequest, CloseAllRequest, CloseEntry};

use super::session_keys::{delete, registration, revocation, session_order};
use super::*;

/// A signed cancel-all for `account`, signed by `signer` with `nonce`, as a JSON body.
fn cancel_all(account: &SigningKey, signer: &SigningKey, nonce: u64) -> Value {
    let account = eip712::address(account.verifying_key());
    let hash = eip712::cancel_all_hash(&eip712::domain(31_337, VAULT), account, nonce, NOW + 60_000);
    let auth = RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(signer, &hash) };
    json!(CancelAllRequest { account, auth: Some(auth) })
}

/// A signed close-all of `markets` quoted at `quoted` each, as a JSON body.
fn close_all(account: &SigningKey, signer: &SigningKey, nonce: u64, markets: &[u32], quoted: i64) -> Value {
    let account = eip712::address(account.verifying_key());
    let closes = markets.iter().map(|&market| CloseEntry { market, quoted: Fixed::from_int(quoted) }).collect();
    let mut request = CloseAllRequest { account, closes, max_slippage_bps: None, auth: None };
    let hash = eip712::close_all_hash(&eip712::domain(31_337, VAULT), &request, nonce, NOW + 60_000);
    request.auth = Some(RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(signer, &hash) });
    json!(request)
}

/// A resting BTC limit buy of 0.1 at 90000 for `account`, signed with `nonce`, as a JSON body.
fn limit(account: &SigningKey, nonce: u64) -> Value {
    let mut request: OrderRequest = serde_json::from_value(order(account, nonce)).unwrap();
    request.kind = OrderKind::Limit { price: Fixed::from_int(90000) };
    request.qty = "0.1".parse().unwrap();
    let hash = eip712::order_hash(&eip712::domain(31_337, VAULT), &request, nonce, NOW + 60_000);
    request.auth = Some(RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(account, &hash) });
    json!(request)
}

fn logged(node: &Node, matches: impl Fn(&Command) -> bool) -> usize {
    node.entries().iter().filter(|entry| matches!(entry, LogEntry::Command(c) if matches(&c.sequenced.cmd))).count()
}

fn types(events: &Value) -> Vec<&str> {
    events.as_array().unwrap().iter().map(|event| event["type"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn cancel_all_and_close_all_are_one_screened_request_and_one_log_entry_each() {
    let node = Node::start("batch-orders", true).await;
    let account = key(1);
    let id = eip712::address(account.verifying_key());
    node.onboard(&account).await;
    let (status, events) = node.post("/v1/orders", order(&account, 1)).await;
    assert_eq!((status, types(&events)), (StatusCode::OK, vec!["order_accepted", "fill"]), "{events}");
    for nonce in [2, 3] {
        assert_eq!(node.post("/v1/orders", limit(&account, nonce)).await.0, StatusCode::OK);
    }
    assert_eq!(node.get(&format!("/v1/accounts/{id}/nonce")).await["next_nonce"], 4);

    // One request cancels both resting orders; the log carries it once, on one nonce.
    let (status, events) = node.post("/v1/orders/cancel-all", cancel_all(&account, &account, 4)).await;
    assert_eq!((status, types(&events)), (StatusCode::OK, vec!["order_cancelled", "order_cancelled"]), "{events}");
    assert_eq!(events[0]["reason"], "client", "{events}");
    assert_eq!(logged(&node, |cmd| matches!(cmd, Command::CancelAll { .. })), 1);
    assert_eq!(node.get(&format!("/v1/accounts/{id}/nonce")).await["next_nonce"], 5);
    // With nothing resting it is not an error.
    let (status, events) = node.post("/v1/orders/cancel-all", cancel_all(&account, &account, 5)).await;
    assert_eq!((status, events), (StatusCode::OK, json!([])));

    // One request closes the position, screened once whatever its length, and logs as one entry.
    let screened = node.source.lock().unwrap().requests.len();
    let (status, events) =
        node.post("/v1/positions/close-all", close_all(&account, &account, 6, &[1, 2], 100000)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(types(&events), ["order_accepted", "fill", "order_rejected"], "{events}");
    assert_eq!(events[2]["client_id"], 1, "the rejection names the entry by its index: {events}");
    assert_eq!(node.source.lock().unwrap().requests.len(), screened + 1, "one screening request");
    assert_eq!(logged(&node, |cmd| matches!(cmd, Command::AccessCloseAll { .. })), 1);
    assert_eq!(node.get(&format!("/v1/accounts/{id}/nonce")).await["next_nonce"], 7);
}

#[tokio::test]
async fn batch_requests_are_refused_for_a_stale_nonce_a_forged_signature_and_a_stranger() {
    let node = Node::start("batch-refusals", true).await;
    let (account, stranger) = (key(1), key(8));
    node.onboard(&account).await;
    let before = node.entries().len();

    let (status, body) = node.post("/v1/orders/cancel-all", cancel_all(&account, &account, 7)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::CONFLICT, Some("invalid nonce")));
    let (status, body) = node.post("/v1/positions/close-all", close_all(&account, &stranger, 1, &[1], 100000)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid signature")));
    let (status, body) = node.post("/v1/orders/cancel-all", cancel_all(&stranger, &stranger, 0)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some("account has never been credited")));
    // A list naming more markets than are listed is refused before its signature is checked: it
    // is not hashed, logged or counted against the nonce.
    let listed = node.state.query(|engine| engine.markets().len()).await.unwrap();
    let too_many: Vec<u32> = (0..=listed as u32).collect();
    let (status, body) =
        node.post("/v1/positions/close-all", close_all(&account, &account, 1, &too_many, 100000)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::BAD_REQUEST, Some("more closes than listed markets")));
    let id = eip712::address(account.verifying_key());
    assert_eq!(node.get(&format!("/v1/accounts/{id}/nonce")).await["next_nonce"], 1);
    // A signed list altered after signing fails the signature, so it never reaches the log.
    let mut altered = close_all(&account, &account, 1, &[1], 100000);
    altered["closes"][0]["quoted"] = json!("1");
    assert_eq!(node.post("/v1/positions/close-all", altered).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(node.entries().len(), before, "none of them reached the log");
}

#[tokio::test]
async fn a_session_key_signs_both_batch_forms_and_a_revoked_one_is_refused_at_the_node() {
    let node = Node::start("batch-session-key", true).await;
    let (account, session) = (key(1), key(7));
    node.onboard(&account).await;
    let register = registration(&account, &session, 1, NOW + 3_600_000);
    assert_eq!(node.post("/v1/session-keys", register).await.0, StatusCode::OK);
    assert_eq!(node.post("/v1/orders", session_order(&account, &session, 0)).await.0, StatusCode::OK);

    let (status, events) = node.post("/v1/orders/cancel-all", cancel_all(&account, &session, 1)).await;
    assert_eq!((status, events), (StatusCode::OK, json!([])));
    let (status, events) = node.post("/v1/positions/close-all", close_all(&account, &session, 2, &[1], 100000)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(types(&events), ["order_accepted", "fill"], "{events}");
    let session_id = eip712::address(session.verifying_key());
    assert_eq!(node.get(&format!("/v1/accounts/{session_id}/nonce")).await["next_nonce"], 3);

    assert_eq!(delete(&node, "/v1/session-keys", revocation(&account, &session, 2)).await.0, StatusCode::OK);
    let (status, body) = node.post("/v1/orders/cancel-all", cancel_all(&account, &session, 3)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("session key revoked")));
}

#[tokio::test]
async fn an_empty_close_all_is_logged_as_a_refusal_so_screening_leaves_an_event() {
    let node = Node::start("batch-empty", true).await;
    let account = key(1);
    node.onboard(&account).await;
    let (status, events) = node.post("/v1/positions/close-all", close_all(&account, &account, 1, &[], 100000)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(types(&events), ["order_rejected"], "{events}");
    assert_eq!(events[0]["reason"]["kind"], "invalid_order", "{events}");
    assert_eq!(logged(&node, |cmd| matches!(cmd, Command::AccessCloseAll { .. })), 1);
}
