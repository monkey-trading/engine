//! API admission before anything is sequenced: a never-credited signer may only declare, and only once
//! a deposit for it is pending; no client can take the whole budget for those declarations however
//! many fresh keys it signs with; an expired request and exhausted screening capacity both refuse
//! without reaching the source or spending a nonce.

use std::num::NonZeroU32;

use super::*;

#[tokio::test]
async fn fresh_signers_declare_only_against_a_pending_deposit_and_within_their_client_share() {
    let mut node = Node::start("access-initial-budget", true).await;
    let mut config = dev_config().api;
    config.per_ip.per_second = NonZeroU32::new(1).unwrap();
    config.per_ip.burst = NonZeroU32::new(2).unwrap();
    config.per_account.per_second = NonZeroU32::new(1).unwrap();
    config.per_account.burst = NonZeroU32::new(1).unwrap();
    node.state.limits = Arc::new(ApiLimits::with_quota(
        &config,
        |rate| governor::Quota::per_hour(rate.per_second).allow_burst(rate.burst),
        governor::clock::DefaultClock::default(),
    ));
    let request = |seed, nonce, expires_at| {
        let client = key(seed);
        let account = eip712::address(client.verifying_key());
        let class = EligibilityClass::Professional;
        let hash = eip712::eligibility_hash(&eip712::domain(31_337, VAULT), account, class, nonce, expires_at);
        EligibilityRequest {
            account,
            class,
            auth: RequestAuth { nonce, expires_at, signature: eip712::sign(&client, &hash) },
        }
    };
    let declare_signed = |client: u8, seed, nonce, expires_at| {
        let peer = Some(Extension(ConnectInfo(SocketAddr::from(([203, 0, 113, client], 9)))));
        crate::api::attest_eligibility(State(node.state.clone()), peer, Json(request(seed, nonce, expires_at)))
    };
    let declare = |client, seed| declare_signed(client, seed, 0, NOW + 60_000);
    let (entries, queries) = (settled_entries(&node).await, node.source.lock().unwrap().requests.len());
    let (status, body) = node.post("/v1/orders", order(&key(5), 0)).await;
    assert_eq!((status, &body["error"]), (StatusCode::FORBIDDEN, &json!("account has never been credited")));
    assert_eq!(settled_entries(&node).await, entries, "a never-credited order reaches no log");
    assert_eq!(node.source.lock().unwrap().requests.len(), queries, "nor the paid source");
    // A key with no pending deposit cannot declare, and spends none of its client's share.
    assert!(matches!(declare(1, 6).await, Err(crate::api::ApiError::Forbidden(_))));
    assert_eq!(settled_entries(&node).await, entries);
    for seed in 1..=4 {
        let account = eip712::address(key(seed).verifying_key());
        let chain_ref = format!("0xpending:{seed}");
        let deposit = node
            .state
            .query(move |engine| {
                crate::test_support::access::funding(
                    engine,
                    Some(account),
                    Some(account),
                    Fixed::from_int(10),
                    &chain_ref,
                )
            })
            .await
            .unwrap();
        assert!(matches!(node.state.submit(deposit).await.unwrap().as_slice(), [Event::DepositPending { .. }]));
    }
    let first = declare(1, 1).await.unwrap();
    assert!(matches!(first.0.as_slice(), [Event::EligibilityAttested { .. }]));
    let before = settled_entries(&node).await;
    // Neither a repeat declaration nor one the engine would refuse for its expiry spends the budget.
    let again = declare_signed(2, 1, 1, NOW + 60_000).await;
    assert!(matches!(again, Err(crate::api::ApiError::Forbidden("account has already declared"))), "{:?}", again.err());
    let too_far = declare_signed(3, 2, 0, NOW + engine_core::auth::MAX_EXPIRY_AHEAD_MS + 1).await;
    assert!(matches!(too_far, Err(crate::api::ApiError::BadRequest(_))), "{:?}", too_far.err());
    assert!(matches!(declare(1, 2).await, Err(crate::api::ApiError::RateLimited(_))), "over its share");
    assert!(declare(2, 3).await.is_ok(), "another client still has the shared budget");
    assert!(matches!(declare(3, 4).await, Err(crate::api::ApiError::RateLimited(_))), "the budget is shared");
    assert_eq!(settled_entries(&node).await, before + 1);
    for seed in [2, 4] {
        let account = eip712::address(key(seed).verifying_key());
        assert_eq!(
            node.state
                .query(move |engine| (engine.next_request_nonce(account), engine.eligibility(account)))
                .await
                .unwrap(),
            (0, None)
        );
    }
    let screened = node.source.lock().unwrap().requests.len();
    assert_eq!(screened, queries + 2, "only the two admitted declarations are screened");
}

#[tokio::test]
async fn exhausted_screening_capacity_neither_calls_the_source_nor_spends_a_client_nonce() {
    let node = Node::start("access-capacity", true).await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    let slots = &node.state.access.as_ref().unwrap().api_slots;
    let occupied = slots.acquire_many(2).await.unwrap();
    let (entries, queries) = (settled_entries(&node).await, node.source.lock().unwrap().requests.len());
    assert_eq!(node.post("/v1/orders", order(&client, 1)).await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(settled_entries(&node).await, entries);
    assert_eq!(node.source.lock().unwrap().requests.len(), queries);
    assert_eq!(node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap(), 1);
    drop(occupied);
    let (status, events) = node.post("/v1/orders", order(&client, 1)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "fill"));
    assert_eq!(node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap(), 2);
}

#[tokio::test]
async fn expired_signed_orders_never_reach_the_source_or_claim_a_public_action() {
    let node = Node::start("access-expired", true).await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    let mut request: OrderRequest = serde_json::from_value(order(&client, 1)).unwrap();
    let hash = eip712::order_hash(&eip712::domain(31_337, VAULT), &request, 1, NOW - 1);
    request.auth = Some(RequestAuth { nonce: 1, expires_at: NOW - 1, signature: eip712::sign(&client, &hash) });
    let (entries, queries) = (settled_entries(&node).await, node.source.lock().unwrap().requests.len());
    let (status, body) = node.post("/v1/orders", json!(request)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], format!("request expired at {}", NOW - 1));
    assert_eq!(settled_entries(&node).await, entries);
    assert_eq!(node.source.lock().unwrap().requests.len(), queries);
    assert_eq!(node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap(), 1);
}
/// The number of logged commands once the writer has flushed everything the sequencer applied:
/// the log flushes behind the replies (and checkpoints flush behind commands), so wait until the
/// logged commands reach the applied seq, and count commands only.
async fn settled_entries(node: &Node) -> usize {
    let applied =
        node.state.query(|engine| engine.last_seq().map_or(0, |seq| seq + 1)).await.expect("the engine answers")
            as usize;
    loop {
        let logged = node.entries().iter().filter(|entry| matches!(entry, LogEntry::Command(_))).count();
        if logged >= applied {
            return logged;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
