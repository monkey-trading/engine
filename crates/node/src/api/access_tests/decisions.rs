//! The API reuses a screening decision for an account and its client address: a request with a fresh
//! decision asks the source nothing, each account and address is screened apart, a refusal is reused
//! like a clearance, an expired decision is asked for again, and the refresh loop keeps an active
//! trader's decision fresh so its next request asks nothing. Before `V85` each request is screened on a
//! decision naming it, and nothing is kept.

use std::net::SocketAddr;

use super::*;
use crate::access::screening_domain;
use crate::clock::Clock;

/// The test config's freshness bound, 1 s, less the tenth a decision keeps for a request's wait.
const USABLE_MS: u64 = 900;

#[tokio::test]
async fn an_account_screened_once_reuses_its_decision_until_it_runs_out() {
    let node = Node::start("decision-reuse", true).await;
    let client = key(1);
    node.onboard(&client).await;
    let calls = source_calls(&node);
    // Two orders and a withdrawal, each at its own nonce, on the declaration's decision.
    for nonce in [1, 2] {
        let (status, events) = node.post("/v1/orders", order(&client, nonce)).await;
        assert_eq!(status, StatusCode::OK, "{events}");
        assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "fill"), "{events}");
    }
    let (status, events) = node.post("/v1/withdrawals", withdrawal(&client, 3)).await;
    let approved = events.as_array().unwrap().iter().any(|event| event["type"] == "withdrawal_approved");
    assert!(status == StatusCode::OK && approved, "{events}");
    assert_eq!(source_calls(&node), calls, "no request asked the source");
    assert_eq!(logged_observations(&node), vec![NOW; 4], "the declaration's decision backs every request");

    // Once it may no longer be attached, the next request asks again and carries the new decision.
    node.move_time(USABLE_MS + 1).await;
    let now = node.clock.now_ms();
    node.source.lock().unwrap().observed_at = now;
    node.post("/v1/orders", order(&client, 4)).await;
    assert_eq!(source_calls(&node), calls + 1);
    assert_eq!(logged_observations(&node).last(), Some(&now));
}

#[tokio::test]
async fn each_account_and_client_address_is_screened_apart() {
    let node = Node::start("decision-pairs", true).await;
    let (first, second) = (key(1), key(2));
    node.onboard(&first).await;
    // Another account at the same address declares on a decision of its own.
    let calls = source_calls(&node);
    node.onboard(&second).await;
    assert_eq!(source_calls(&node), calls + 1);
    // The first account at another address is screened once there.
    let elsewhere = SocketAddr::from(([127, 0, 0, 2], 9));
    for nonce in [1, 2] {
        let (status, events) = post_from(&node, elsewhere, "/v1/orders", order(&first, nonce)).await;
        assert_eq!(status, StatusCode::OK, "{events}");
    }
    assert_eq!(source_calls(&node), calls + 2);
    assert_eq!(node.source.lock().unwrap().requests.last().unwrap()["peer"], "127.0.0.2");
    // Each account's orders from the first address reuse its own decision.
    for (client, nonce) in [(&first, 3), (&second, 1)] {
        let (status, events) = node.post("/v1/orders", order(client, nonce)).await;
        assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "fill"), "{status} {events}");
    }
    assert_eq!(source_calls(&node), calls + 2);
}

#[tokio::test]
async fn a_refused_decision_is_reused_without_asking_the_source_again() {
    let node = Node::start("decision-refused", true).await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    node.source.lock().unwrap().sanctioned.insert(account);
    node.forget_decisions();
    let calls = source_calls(&node);
    let refused = |events: &Value| events[0]["reason"]["reason"].clone();
    assert_eq!(refused(&node.post("/v1/orders", order(&client, 1)).await.1), "account_sanctioned");
    assert_eq!(refused(&node.post("/v1/orders", order(&client, 2)).await.1), "account_sanctioned");
    assert_eq!(refused(&node.post("/v1/withdrawals", withdrawal(&client, 3)).await.1), "account_sanctioned");
    assert_eq!(source_calls(&node), calls + 1, "the refusal is asked for once");
    assert_eq!(node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap(), 4);
}

#[tokio::test]
async fn the_refresh_loop_keeps_an_active_traders_decision_fresh() {
    let node = Node::start("decision-refresh", true).await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    let access = node.state.access.clone().unwrap();
    let domain = screening_domain(&node.state.auth, &node.state.operator);
    let refreshes = tokio::spawn(access.clone().refresh(domain, node.clock.clone()));
    let calls = source_calls(&node);
    // Half the bound on, the pair that declared is due a refresh, which the source answers with a
    // newer decision. Nothing is due before then.
    let refreshed_at = NOW + 500;
    node.source.lock().unwrap().observed_at = refreshed_at;
    node.move_time(500).await;
    let past_first = refreshed_at + USABLE_MS as i64 - 499;
    tokio::time::timeout(Duration::from_secs(10), async {
        while access.api_decisions.hit(account, [127, 0, 0, 1].into(), past_first).is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the refresh keeps a newer decision");
    let refreshed = node.source.lock().unwrap().requests[calls..].to_vec();
    assert_eq!(refreshed, [json!({ "account": account, "payer": null, "peer": "127.0.0.1" })]);
    // Past the first decision's life, the order is screened on the refreshed one without asking.
    node.move_time(USABLE_MS - 500 + 1).await;
    let (status, events) = node.post("/v1/orders", order(&client, 1)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(source_calls(&node), calls + 1);
    assert_eq!(logged_observations(&node).last(), Some(&refreshed_at));
    refreshes.abort();
}

#[tokio::test]
async fn before_v85_each_request_is_screened_on_a_decision_naming_it() {
    let genesis = dev_config().genesis;
    let node = Node::start_from("decision-v84", true, true, false, &genesis, ProtocolVersion::V84).await;
    let client = key(1);
    node.onboard(&client).await;
    let calls = source_calls(&node);
    for nonce in [1, 2] {
        let (status, events) = node.post("/v1/orders", order(&client, nonce)).await;
        assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "fill"), "{status} {events}");
    }
    let (status, events) = node.post("/v1/withdrawals", withdrawal(&client, 3)).await;
    let approved = events.as_array().unwrap().iter().any(|event| event["type"] == "withdrawal_approved");
    assert!(status == StatusCode::OK && approved, "{events}");
    // Each request asked the source about itself, and nothing was kept for the next.
    let requests = node.source.lock().unwrap().requests[calls..].to_vec();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|request| request["request_hash"].is_string()), "{requests:?}");
    assert!(!node.state.access.as_ref().unwrap().api_decisions.reusing());
}

// ── Helpers ──

impl Node {
    /// Forgets the API's screening decisions, as once each runs out, so the next request of each
    /// account asks the source, which a test has just changed.
    pub(super) fn forget_decisions(&self) {
        self.state.access.as_ref().expect("access is configured").api_decisions.forget();
    }
}

/// The calls the source has answered.
fn source_calls(node: &Node) -> usize {
    node.source.lock().unwrap().requests.len()
}

/// The `observed_at` of each client request's decision the log holds, in order.
fn logged_observations(node: &Node) -> Vec<i64> {
    node.entries()
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(command) => match &command.sequenced.cmd {
                Command::AttestEligibility { evidence, .. }
                | Command::AccessOrder { evidence, .. }
                | Command::AccessWithdraw { evidence, .. } => match evidence {
                    engine_core::access::AccessEvidence::Screened(screening) => Some(screening.observed_at),
                    engine_core::access::AccessEvidence::Failed { .. } => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// `node.post` from the client at `peer`.
async fn post_from(node: &Node, peer: SocketAddr, path: &str, body: Value) -> (StatusCode, Value) {
    let app = router(node.state.clone()).layer(Extension(ConnectInfo(peer)));
    let request = Request::post(path).header("content-type", "application/json").body(Body::from(body.to_string()));
    let response = app.oneshot(request.unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}
