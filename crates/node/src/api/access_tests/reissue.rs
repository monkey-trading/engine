//! A reissue request is screened like a new withdrawal, by account and transport peer, and a refusal
//! is a 403 that leaves the log untouched.

use super::*;

#[tokio::test]
async fn a_reissue_is_refused_for_an_account_sanctioned_or_geo_blocked_since_its_approval() {
    let node = Node::start("reissue-screening", true).await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    let nonce = node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap();
    let approved = node.post("/v1/withdrawals", withdrawal(&client, nonce)).await.1;
    assert!(approved.as_array().unwrap().iter().any(|event| event["type"] == "withdrawal_approved"), "{approved}");
    let hash = crate::api::reissue::reissue_hash(&eip712::domain(31_337, VAULT), account, 1, 0);
    let body = json!({ "replaces_expires_at": 0, "signature": eip712::sign(&client, &hash) });
    let path = format!("/v1/accounts/{account}/withdrawals/1/reissue");
    let before = node.entries();
    let queries = node.source.lock().unwrap().requests.len();
    node.source.lock().unwrap().sanctioned.insert(account);
    let (status, refused) = node.post(&path, body.clone()).await;
    assert_eq!(
        (status, refused),
        (StatusCode::FORBIDDEN, json!({"error":"access refused","reason":"account_sanctioned"}))
    );
    node.source.lock().unwrap().sanctioned.clear();
    node.source.lock().unwrap().jurisdiction = Jurisdiction::UnitedStates;
    let (status, refused) = node.post(&path, body.clone()).await;
    assert_eq!((status, refused), (StatusCode::FORBIDDEN, json!({"error":"access refused","reason":"united_states"})));
    // Allowed, it reaches the payout schedule, which mock custody does not have.
    node.source.lock().unwrap().jurisdiction = Jurisdiction::Allowed;
    assert_eq!(node.post(&path, body).await.0, StatusCode::SERVICE_UNAVAILABLE);
    let requests = node.source.lock().unwrap().requests[queries..].to_vec();
    assert_eq!(requests.len(), 3);
    for request in requests {
        assert_eq!(serde_json::from_value::<eip712::B256>(request["request_hash"].clone()).unwrap(), hash);
        assert_eq!((request["account"].clone(), request["peer"].clone()), (json!(account), json!("127.0.0.1")));
    }
    assert_eq!(node.entries(), before, "a reissue refusal sequences nothing");
}
