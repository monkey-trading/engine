//! What a never-credited account's free requests spend: one token from its client's share and one
//! from the budget all clients share, with nothing spent by a request refused before the log.

use std::num::NonZeroU32;

use axum::http::{Method, header};
use engine_core::auth::MAX_EXPIRY_AHEAD_MS;

use super::admission::settled_entries;
use super::pre_deposit::{
    HOUR_MS, address, deposit, key_management_auth, next_nonce, registration_hash, revocation_hash, screened,
};
use super::session_keys::{registration, revocation};
use super::*;
use crate::trusted_time::tests::logs::Logs;

#[tokio::test]
async fn a_stale_expired_or_too_far_request_spends_no_nonce_and_no_budget() {
    // Two tokens for the client: one for the declaration, one for the registration.
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
    // Three tokens per client, five shared, none refilling within the test.
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
        assert_eq!((status, &body), (limited, &json!({ "error": crate::api::SETUP_LIMITED })));
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

#[tokio::test]
async fn a_wallet_with_a_pending_deposit_declares_past_an_exhausted_budget_and_spends_none_of_it() {
    // One token for the shared budget and one per client, none refilling within the test.
    let node = limited(Node::start("pre-deposit-paid", true).await, 2, 1);
    let (free, paid, unpaid, late, other, session) = (key(1), key(2), key(3), key(5), key(4), key(7));
    // A deposit for another account does not make a wallet paid.
    deposit(&node, address(&other), "0xfunding:other").await;
    deposit(&node, address(&paid), "0xfunding:paid").await;

    let post = Method::POST;
    let path = "/v1/session-keys";
    let (ok, limited) = (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(send(&node, 1, post.clone(), "/v1/eligibility", declaration(&free, 0)).await.0, ok);
    // Client 1 is out of its share, which it would be for any unpaid wallet.
    let (status, body, retry_after) = send(&node, 1, post.clone(), "/v1/eligibility", declaration(&unpaid, 0)).await;
    assert_eq!((status, body["error"].as_str()), (limited, Some(crate::api::SETUP_LIMITED)));
    assert!(retry_after.is_some_and(|seconds| seconds >= 1), "{retry_after:?}");
    // The paid wallet declares from the same client, and took nothing from the shared budget: a
    // client that has not used its share still takes the last shared token.
    let queries = screened(&node);
    assert_eq!(send(&node, 1, post.clone(), "/v1/eligibility", declaration(&paid, 0)).await.0, ok);
    assert_eq!(screened(&node), queries + 1, "the paid wallet is still screened");
    assert_eq!(send(&node, 2, post.clone(), "/v1/eligibility", declaration(&unpaid, 0)).await.0, ok);
    // The shared budget is out now; the paid wallet's keys, which are not exempt, wait with everyone.
    let register = registration(&paid, &session, 1, NOW + HOUR_MS);
    assert_eq!(send(&node, 3, post.clone(), path, register).await.0, limited);
    // A wallet with no deposit for it is refused, whoever else has one, and the paid wallet's second
    // declaration is refused as before, before its rate.
    assert_eq!(send(&node, 4, post.clone(), "/v1/eligibility", declaration(&late, 0)).await.0, limited);
    assert_eq!(send(&node, 5, post, "/v1/eligibility", declaration(&paid, 1)).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_paid_wallets_refused_declarations_are_held_to_its_own_bucket_and_spend_no_budget() {
    // Four shared tokens and two per client, none refilling within the test.
    let node = limited(Node::start("pre-deposit-paid-refused", true).await, 4, 2);
    let (first, second, paid, later) = (key(1), key(2), key(3), key(4));
    deposit(&node, address(&paid), "0xfunding:paid").await;
    let post = Method::POST;
    let declare =
        |client, wallet, nonce| send(&node, client, post.clone(), "/v1/eligibility", declaration(wallet, nonce));
    // Client 1 spends its share.
    assert_eq!(declare(1, &first, 0).await.0, StatusCode::OK);
    assert_eq!(declare(1, &second, 0).await.0, StatusCode::OK);
    assert_eq!(declare(1, &later, 0).await.0, StatusCode::TOO_MANY_REQUESTS);

    // The source refuses the paid wallet's region, so the engine records the refusal and the wallet
    // stays undeclared, paid and exempt from client 1's share.
    node.source.lock().unwrap().jurisdiction = Jurisdiction::UnitedStates;
    for nonce in 0..2 {
        let (status, events, _) = declare(1, &paid, nonce).await;
        assert_eq!((status, &events[0]["type"]), (StatusCode::OK, &json!("eligibility_rejected")), "{events}");
    }
    // Its own bucket is out: the third attempt comes from a fresh client, with the paid budget
    // still holding, so only the account's rate can refuse it before the source or the log.
    let (entries, queries) = (settled_entries(&node).await, screened(&node));
    let (status, body, retry_after) = declare(2, &paid, 2).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some(crate::api::SETUP_LIMITED)));
    assert!(retry_after.is_some_and(|seconds| seconds >= 1), "{retry_after:?}");
    assert_eq!((settled_entries(&node).await, screened(&node)), (entries, queries));
    assert_eq!(next_nonce(&node, address(&paid)).await, 2, "the refused attempt used no nonce");
    // The refusals took nothing from the shared budget: two of its four tokens remain.
    node.source.lock().unwrap().jurisdiction = Jurisdiction::Allowed;
    assert_eq!(declare(2, &later, 0).await.0, StatusCode::OK);
}

#[tokio::test]
async fn the_paid_budget_running_out_is_an_alert_and_one_clients_refused_wallets_cannot_drain_it() {
    let (logs, _guard) = Logs::capture();
    // Two shared tokens and two per client, none refilling within the test.
    let node = limited(Node::start("pre-deposit-paid-alert", true).await, 2, 2);
    let wallets = [key(1), key(2), key(3), key(4)];
    for (n, wallet) in wallets.iter().enumerate() {
        deposit(&node, address(wallet), &format!("0xfunding:paid{n}")).await;
    }
    node.source.lock().unwrap().jurisdiction = Jurisdiction::UnitedStates;
    let declare =
        |client, wallet, nonce| send(&node, client, Method::POST, "/v1/eligibility", declaration(wallet, nonce));
    // Client 1's two paid tokens go on two wallets; its third wallet is refused by its client share, which
    // is no alert.
    assert_eq!(declare(1, &wallets[0], 0).await.0, StatusCode::OK);
    assert_eq!(declare(1, &wallets[1], 0).await.0, StatusCode::OK);
    assert_eq!(declare(1, &wallets[2], 0).await.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(logs.count("alert=\"api_paid_declaration_budget_full\""), 0, "one client's share is its own problem");
    // The two shared tokens are spent, so another client's wallet is refused, and the operator hears of it once.
    assert_eq!(declare(2, &wallets[2], 0).await.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(declare(2, &wallets[3], 0).await.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(logs.count("alert=\"api_paid_declaration_budget_full\""), 1);
}

// ── Helpers ──

/// `node` with fresh-account budgets of `shared` tokens and `share` per client, none refilling within
/// a test.
fn limited(mut node: Node, shared: u32, share: u32) -> Node {
    let mut config = dev_config().api;
    let budget = |burst| Budget { per_minute: NonZeroU32::MIN, burst: NonZeroU32::new(burst).unwrap() };
    config.fresh_accounts = FreshAccountBudgets { shared: budget(shared), per_client: budget(share) };
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
