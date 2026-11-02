//! A reissue request is screened like a new withdrawal, by account and transport peer, and a refusal
//! is a 403 that leaves the log untouched.

use super::*;

impl Node {
    /// A node with the checkpoint gate EVM custody has, its collateral reconciled, so a withdrawal's
    /// approval is held as an unpaid payout that can be reissued.
    async fn start_gated(name: &str) -> Self {
        let node = Self::start_with_time(name, true, true, true).await;
        let status = sequencer::Request::CheckpointStatus {
            posted: None,
            latest: None,
            health: Some(crate::checkpoints::Health::Healthy),
            lifecycle: None,
        };
        node.state.tx.send(status).await.unwrap();
        node
    }
}

#[tokio::test]
async fn a_reissue_is_refused_for_an_account_sanctioned_or_geo_blocked_since_its_approval() {
    let node = Node::start_gated("reissue-screening").await;
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
    // Allowed, it reaches the payout schedule and queues.
    node.source.lock().unwrap().jurisdiction = Jurisdiction::Allowed;
    let (status, queued) = node.post(&path, body).await;
    assert_eq!((status, queued["status"].as_str()), (StatusCode::OK, Some("queued")), "{queued}");
    let requests = node.source.lock().unwrap().requests[queries..].to_vec();
    assert_eq!(requests.len(), 3);
    for request in requests {
        assert_eq!(serde_json::from_value::<eip712::B256>(request["request_hash"].clone()).unwrap(), hash);
        assert_eq!((request["account"].clone(), request["peer"].clone()), (json!(account), json!("127.0.0.1")));
    }
    assert_eq!(node.entries(), before, "a reissue refusal sequences nothing");
}

#[tokio::test]
async fn a_reissue_for_an_approval_that_does_not_exist_is_refused_before_it_screens() {
    let node = Node::start_gated("reissue-unknown").await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    let nonce = node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap();
    assert_eq!(node.post("/v1/withdrawals", withdrawal(&client, nonce)).await.0, StatusCode::OK);
    let reissue = |signer: &SigningKey, payout_nonce: u64| {
        let account = eip712::address(signer.verifying_key());
        let hash = crate::api::reissue::reissue_hash(&eip712::domain(31_337, VAULT), account, payout_nonce, 0);
        let body = json!({ "replaces_expires_at": 0, "signature": eip712::sign(signer, &hash) });
        (format!("/v1/accounts/{account}/withdrawals/{payout_nonce}/reissue"), body)
    };
    let (before, queries) = (node.entries(), node.source.lock().unwrap().requests.len());

    // Keys that never held an approval, and a nonce the real account never was given, spend no
    // screening call, however many ask.
    let mut unknown = (2..14).map(|seed| (key(seed), 1)).collect::<Vec<_>>();
    unknown.push((client.clone(), 2));
    for (signer, payout_nonce) in unknown {
        let (path, body) = reissue(&signer, payout_nonce);
        assert_eq!(node.post(&path, body).await.0, StatusCode::NOT_FOUND);
    }
    assert_eq!(node.source.lock().unwrap().requests.len(), queries, "no request was screened");
    assert_eq!(node.entries(), before, "a refused reissue sequences nothing");

    // The account that holds the approval is screened and queued as before.
    let (path, body) = reissue(&client, 1);
    assert_eq!(node.post(&path, body).await.0, StatusCode::OK);
    assert_eq!(node.source.lock().unwrap().requests.len(), queries + 1);
}
