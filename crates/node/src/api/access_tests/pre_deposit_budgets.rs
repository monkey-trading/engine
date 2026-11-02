//! What a never-credited account's free requests spend: one token from its client's share and one
//! from the budget all clients share, with nothing spent by a request refused before the log.

use std::num::NonZeroU32;

use axum::http::{Method, header};
use engine_core::auth::MAX_EXPIRY_AHEAD_MS;

use super::admission::settled_entries;
use super::pre_deposit::{
    HOUR_MS, address, key_management_auth, next_nonce, registration_hash, revocation_hash, screened,
};
use super::session_keys::{registration, revocation};
use super::*;
use crate::trusted_time::tests::logs::Logs;

#[tokio::test]
async fn a_stale_expired_or_too_far_request_spends_no_nonce_and_no_budget() {
    // Two tokens for the client and the account: one for the declaration, one for the registration.
    let node = limited(Node::start("pre-deposit-fresh", true).await, 20, 2);
    let (wallet, session) = (key(1), key(7));
    let account = address(&wallet);
    let client = 1;
    let entries = settled_entries(&node).await;

    // Refused before the budgets, the declaration's attempts leave its token for the valid one.
    for (nonce, expires_at, status, message) in fresh_refusals() {
        let hash = eip712::eligibility_hash(
            &eip712::domain(31_337, VAULT),
            account,
            EligibilityClass::Professional,
            nonce,
            expires_at,
        );
        let request = EligibilityRequest {
            account,
            class: EligibilityClass::Professional,
            auth: RequestAuth { nonce, expires_at, signature: eip712::sign(&wallet, &hash) },
        };
        let (got, body, _) = send(&node, client, Method::POST, "/v1/eligibility", json!(request)).await;
        assert_eq!((got, body["error"].as_str()), (status, Some(message.as_str())));
    }
    let (status, _, _) = send(&node, client, Method::POST, "/v1/eligibility", declaration(&wallet, 0)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(settled_entries(&node).await, entries + 1);

    // The same refusals on both key routes spend nothing, so the last token admits one request.
    for (nonce, expires_at, status, message) in fresh_refusals() {
        let nonce = nonce + 1;
        let mut register = registration(&wallet, &session, nonce, NOW + HOUR_MS);
        register["auth"] = key_management_auth(&wallet, registration_hash(&wallet, &session, nonce, expires_at), nonce);
        register["auth"]["expires_at"] = json!(expires_at);
        let (got, body, _) = send(&node, client, Method::POST, "/v1/session-keys", register).await;
        assert_eq!((got, body["error"].as_str()), (status, Some(message.as_str())), "registration");
        let mut revoke = revocation(&wallet, &session, nonce);
        revoke["auth"] = key_management_auth(&wallet, revocation_hash(&wallet, &session, nonce, expires_at), nonce);
        revoke["auth"]["expires_at"] = json!(expires_at);
        let (got, body, _) = send(&node, client, Method::DELETE, "/v1/session-keys", revoke).await;
        assert_eq!((got, body["error"].as_str()), (status, Some(message.as_str())), "revocation");
    }
    assert_eq!(next_nonce(&node, account).await, 1);
    assert_eq!(settled_entries(&node).await, entries + 1);
    let register = registration(&wallet, &session, 1, NOW + HOUR_MS);
    assert_eq!(send(&node, client, Method::POST, "/v1/session-keys", register).await.0, StatusCode::OK);
    let revoke = revocation(&wallet, &session, 2);
    assert_eq!(send(&node, client, Method::DELETE, "/v1/session-keys", revoke).await.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(settled_entries(&node).await, entries + 2);
}

#[tokio::test]
async fn each_pre_deposit_request_takes_a_token_from_its_clients_share_and_the_shared_budget() {
    let (logs, _guard) = Logs::capture();
    let node = Node::start("pre-deposit-budget", true).await;
    let credited = key(9);
    node.onboard(&credited).await;
    // Three tokens per client and per account, five shared, none refilling within the test.
    let node = limited(node, 5, 3);
    let (first, second, fresh, session) = (key(1), key(2), key(3), key(7));
    let (entries, queries) = (settled_entries(&node).await, screened(&node));

    // Each of the three kinds counts against the client, through the route's own client address.
    let post = Method::POST;
    let remove = Method::DELETE;
    let path = "/v1/session-keys";
    let ok = StatusCode::OK;
    let limited = StatusCode::TOO_MANY_REQUESTS;
    assert_eq!(send(&node, 1, post.clone(), "/v1/eligibility", declaration(&first, 0)).await.0, ok);
    assert_eq!(send(&node, 1, post.clone(), path, registration(&first, &session, 1, NOW + HOUR_MS)).await.0, ok);
    assert_eq!(send(&node, 1, remove.clone(), path, revocation(&first, &session, 2)).await.0, ok);
    // Over its share, a client's fresh signing key gets nothing and spends none of the shared budget.
    assert_eq!(send(&node, 1, post.clone(), "/v1/eligibility", declaration(&second, 0)).await.0, limited);
    assert_eq!(send(&node, 1, post.clone(), path, registration(&first, &session, 3, NOW + HOUR_MS)).await.0, limited);
    assert_eq!(logs.count("alert=\"api_initial_budget_full\""), 0, "one client's share is its own problem");
    assert_eq!(send(&node, 2, post.clone(), "/v1/eligibility", declaration(&second, 0)).await.0, ok);
    assert_eq!(send(&node, 3, post.clone(), path, registration(&second, &session, 1, NOW + HOUR_MS)).await.0, ok);
    assert_eq!(settled_entries(&node).await, entries + 5);

    // The shared budget is out: what a new wallet sees, whichever client or fresh key it uses.
    for (client, method, route, request) in [
        (4, post.clone(), "/v1/eligibility", declaration(&fresh, 0)),
        (5, post.clone(), path, registration(&second, &session, 2, NOW + HOUR_MS)),
        (6, remove.clone(), path, revocation(&second, &session, 2)),
    ] {
        let (status, body, retry_after) = send(&node, client, method, route, request).await;
        assert_eq!((status, &body), (limited, &json!({ "error": "rate limited" })));
        assert!(retry_after.is_some_and(|seconds| seconds >= 1), "{retry_after:?}");
    }
    assert_eq!(settled_entries(&node).await, entries + 5);
    assert_eq!(screened(&node), queries + 2, "only the admitted declarations are screened");
    assert_eq!(logs.count("alert=\"api_initial_budget_full\""), 1, "the operator hears of it once per episode");

    // A credited wallet spends neither budget, so it keeps managing its keys.
    let nonce = next_nonce(&node, address(&credited)).await;
    let register = registration(&credited, &session, nonce, NOW + HOUR_MS);
    assert_eq!(send(&node, 4, post.clone(), path, register).await.0, ok);
    let (status, events, _) = send(&node, 4, post, "/v1/orders", order(&credited, nonce + 1)).await;
    assert_eq!(status, ok, "{events}");
}

// ── Helpers ──

/// `node` with fresh-account budgets of `shared` tokens and `share` per client, none refilling within
/// a test. The budgets take their rates from `per_ip` and `per_account`, so an account also gets
/// `share` tokens and a client `shared` for all its requests.
fn limited(mut node: Node, shared: u32, share: u32) -> Node {
    let mut config = dev_config().api;
    config.per_ip = RateLimit { per_second: NonZeroU32::MIN, burst: NonZeroU32::new(shared).unwrap() };
    config.per_account = RateLimit { per_second: NonZeroU32::MIN, burst: NonZeroU32::new(share).unwrap() };
    node.state.limits = Arc::new(ApiLimits::with_quota(
        &config,
        |rate| Quota::per_hour(rate.per_second).allow_burst(rate.burst),
        DefaultClock::default(),
    ));
    node
}

/// The status, JSON body and `Retry-After` seconds of `method path` from client `203.0.113.<client>`,
/// through the node's routes under its current limits.
pub(super) async fn send(
    node: &Node,
    client: u8,
    method: Method,
    path: &str,
    body: Value,
) -> (StatusCode, Value, Option<u64>) {
    let peer = ConnectInfo(SocketAddr::from(([203, 0, 113, client], 9)));
    let app = router(node.state.clone()).layer(Extension(peer));
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let retry_after = response.headers().get(header::RETRY_AFTER).map(|value| value.to_str().unwrap().parse().unwrap());
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap(), retry_after)
}

/// `(nonce, expires_at, status, error)` of a first-nonce request refused before the budgets: a stale
/// nonce, an expired request, and an expiry past the bound the engine would refuse.
fn fresh_refusals() -> [(u64, i64, StatusCode, String); 3] {
    let latest = NOW + MAX_EXPIRY_AHEAD_MS;
    [
        (5, NOW + 60_000, StatusCode::CONFLICT, "invalid nonce".to_owned()),
        (0, NOW - 1, StatusCode::BAD_REQUEST, format!("request expired at {}", NOW - 1)),
        (0, latest + 1, StatusCode::BAD_REQUEST, format!("a never-credited account's request must expire by {latest}")),
    ]
}
