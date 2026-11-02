use super::*;

#[tokio::test]
async fn a_session_key_trades_for_its_account_until_revoked() {
    let node = Node::start("session-keys", true).await;
    let account = key(1);
    let session = key(7);
    node.onboard(&account).await;

    // The account registers the key with one signature; the key then trades without the wallet.
    let register = registration(&account, &session, 1, NOW + 3_600_000);
    let (status, events) = node.post("/v1/session-keys", register).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(events[0]["type"], "session_key_registered", "{events}");

    let (status, events) = node.post("/v1/orders", session_order(&account, &session, 0)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "fill"), "{events}");

    // The account's sequence moved for its declaration and the registration; the key's is its own.
    let account_id = eip712::address(account.verifying_key());
    let session_id = eip712::address(session.verifying_key());
    assert_eq!(node.get(&format!("/v1/accounts/{account_id}/nonce")).await["next_nonce"], 2);
    assert_eq!(node.get(&format!("/v1/accounts/{session_id}/nonce")).await["next_nonce"], 1);

    // A withdrawal signed by the key reaches the log and refuses as out of scope.
    let (status, events) = node.post("/v1/withdrawals", session_withdrawal(&account, &session, 1)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let rejection = events.as_array().unwrap().iter().find(|e| e["type"] == "withdrawal_rejected").unwrap();
    assert_eq!(rejection["reason"]["kind"], "session_key_scope", "{events}");

    // An account-signed revocation kills the key at the node: its next request is unauthorized.
    let revoke = revocation(&account, &session, 2);
    let (status, events) = delete(&node, "/v1/session-keys", revoke).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(events[0]["type"], "session_key_revoked", "{events}");
    let (status, body) = node.post("/v1/orders", session_order(&account, &session, 2)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("session key revoked")));

    // An unknown key is an invalid signature.
    let stranger = key(8);
    let (status, body) = node.post("/v1/orders", session_order(&account, &stranger, 0)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid signature")));
}

#[tokio::test]
async fn an_expired_session_key_is_refused_at_the_node() {
    let node = Node::start("session-key-expiry", true).await;
    let account = key(1);
    let session = key(7);
    node.onboard(&account).await;
    // Registered with a one-minute expiry; the request itself expires in a minute too.
    let register = registration(&account, &session, 1, NOW + 60_000);
    assert_eq!(node.post("/v1/session-keys", register).await.0, StatusCode::OK);

    // Past the key's expiry the node refuses, distinctly from an unknown key.
    node.move_time(2 * 60_000).await;
    let (status, body) = node.post("/v1/orders", session_order_at(&account, &session, 0, NOW + 3 * 60_000)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("session key expired")));
}

#[tokio::test]
async fn a_session_key_cannot_register_another_key() {
    let node = Node::start("session-key-scope", true).await;
    let account = key(1);
    let session = key(7);
    node.onboard(&account).await;
    let register = registration(&account, &session, 1, NOW + 3_600_000);
    assert_eq!(node.post("/v1/session-keys", register).await.0, StatusCode::OK);

    // The key's registration of a stranger is not the account's signature: the API refuses it
    // before sequencing, so it never reaches the log, and no nonce is spent.
    let stranger = key(8);
    let mut body = registration(&account, &stranger, 0, NOW + 3_600_000);
    body["auth"] = signed_registration_auth(&account, &stranger, 0, NOW + 3_600_000, &session);
    let (status, body) = node.post("/v1/session-keys", body).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid signature")));
    let session_id = eip712::address(session.verifying_key());
    assert_eq!(node.get(&format!("/v1/accounts/{session_id}/nonce")).await["next_nonce"], 0);
    assert!(
        node.entries().iter().all(|entry| !matches!(entry, LogEntry::Command(c) if matches!(c.sequenced.cmd, Command::RegisterSessionKey { session_key, .. } if session_key == eip712::address(stranger.verifying_key())))),
        "the stranger's registration never reached the log"
    );
}

#[tokio::test]
async fn a_session_key_cancels_and_a_stale_nonce_stays_out_of_the_log() {
    let node = Node::start("session-key-cancel", true).await;
    let account = key(1);
    let session = key(7);
    node.onboard(&account).await;
    let register = registration(&account, &session, 1, NOW + 3_600_000);
    assert_eq!(node.post("/v1/session-keys", register).await.0, StatusCode::OK);

    // The key places and cancels a resting order with its own nonce sequence.
    let mut limit: OrderRequest = serde_json::from_value(session_order(&account, &session, 0)).unwrap();
    limit.kind = OrderKind::Limit { price: "90000".parse().unwrap() };
    let hash = eip712::order_hash(&eip712::domain(31_337, VAULT), &limit, 0, NOW + 60_000);
    limit.auth = Some(RequestAuth { nonce: 0, expires_at: NOW + 60_000, signature: eip712::sign(&session, &hash) });
    let (status, events) = node.post("/v1/orders", serde_json::to_value(&limit).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let order_id = events[0]["order_id"].as_u64().unwrap();
    let hash = eip712::cancel_hash(
        &eip712::domain(31_337, VAULT),
        eip712::address(account.verifying_key()),
        order_id,
        1,
        NOW + 60_000,
    );
    let auth = RequestAuth { nonce: 1, expires_at: NOW + 60_000, signature: eip712::sign(&session, &hash) };
    let body = serde_json::json!({
        "account": eip712::address(account.verifying_key()),
        "order_id": order_id,
        "auth": auth,
    });
    let (status, events) = node.post("/v1/orders/cancel", body).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(events[0]["type"], "order_cancelled", "{events}");

    // An order whose nonce the cancel used conflicts and never reaches the log.
    let stale = session_order(&account, &session, 1);
    let (status, body) = node.post("/v1/orders", stale).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::CONFLICT, Some("invalid nonce")));
    let logged = node
        .entries()
        .iter()
        .filter(|entry| matches!(entry, LogEntry::Command(c) if matches!(c.sequenced.cmd, Command::AccessOrder { .. })))
        .count();
    assert_eq!(logged, 1, "only the first order reached the log");
}

#[tokio::test]
async fn a_never_credited_account_cannot_register_a_session_key() {
    let node = Node::start("session-key-uncredited", true).await;
    let account = key(1);
    let session = key(7);
    // No deposit has ever credited the account, so even its own signed registration is refused
    // before sequencing, and nothing enters the log.
    let register = registration(&account, &session, 0, NOW + 3_600_000);
    let (status, body) = node.post("/v1/session-keys", register).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some("account has never been credited")));
    assert!(
        node.entries().iter().all(|entry| !matches!(entry, LogEntry::Command(c) if matches!(c.sequenced.cmd, Command::RegisterSessionKey { .. }))),
        "the registration never reached the log"
    );
}

#[tokio::test]
async fn both_session_key_routes_refuse_a_missing_signature() {
    let node = Node::start("session-key-missing-auth", true).await;
    let account = key(1);
    let session = key(7);
    node.onboard(&account).await;
    let mut register = registration(&account, &session, 1, NOW + 3_600_000);
    register.as_object_mut().unwrap().remove("auth");
    let (status, body) = node.post("/v1/session-keys", register).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("missing signature")));

    let register = registration(&account, &session, 1, NOW + 3_600_000);
    assert_eq!(node.post("/v1/session-keys", register).await.0, StatusCode::OK);
    let mut revoke = revocation(&account, &session, 2);
    revoke.as_object_mut().unwrap().remove("auth");
    let (status, body) = delete(&node, "/v1/session-keys", revoke).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("missing signature")));
    let account_id = eip712::address(account.verifying_key());
    assert_eq!(node.get(&format!("/v1/accounts/{account_id}/nonce")).await["next_nonce"], 2);
}

#[tokio::test]
async fn a_flooding_key_cannot_starve_the_accounts_revocation() {
    let node = Node::start("session-key-rate", true).await;
    let account = key(1);
    let session = key(7);
    let other = key(8);
    node.onboard(&account).await;
    for (nonce, key) in [(1, &session), (2, &other)] {
        let register = registration(&account, key, nonce, NOW + 3_600_000);
        assert_eq!(node.post("/v1/session-keys", register).await.0, StatusCode::OK);
    }
    // The account's keys share one session bucket at the per-account rate, whatever key signs:
    // exhausting it with alternating keys shows keys do not multiply the allowance.
    let burst = u64::from(dev_config().api.per_account.burst.get());
    let mut nonces = [0u64; 2];
    for index in 0..burst {
        let slot = usize::try_from(index % 2).unwrap();
        let key = if index % 2 == 0 { &session } else { &other };
        let (status, events) = node.post("/v1/orders", session_order(&account, key, nonces[slot])).await;
        assert_eq!(status, StatusCode::OK, "{events}");
        nonces[slot] += 1;
    }
    // The shared session bucket is empty and does not refill within the test, so ten more
    // requests, whichever key signs them, are all refused.
    let mut refused = 0;
    for index in 0..10 {
        let slot = usize::try_from(index % 2).unwrap();
        let key = if index % 2 == 0 { &session } else { &other };
        let (status, _body) = node.post("/v1/orders", session_order(&account, key, nonces[slot])).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "request {index} was not refused");
        refused += 1;
    }
    assert_eq!(refused, 10);
    // The wallet's own allowance is untouched: its signed revocation sequences at once.
    let revoke = revocation(&account, &session, 3);
    let (status, events) = delete(&node, "/v1/session-keys", revoke).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(events[0]["type"], "session_key_revoked", "{events}");
}

#[tokio::test]
async fn a_key_expired_by_the_clock_is_refused_before_sequencing() {
    let node = Node::start("session-key-clock-expiry", true).await;
    let account = key(1);
    let session = key(7);
    node.onboard(&account).await;
    let register = registration(&account, &session, 1, NOW + 60_000);
    assert_eq!(node.post("/v1/session-keys", register).await.0, StatusCode::OK);

    // The clock passes the key's expiry while no command advances the sequenced time past it;
    // the request itself is still fresh. The node refuses before sequencing, spending nothing.
    node.clock.advance(Duration::from_millis(2 * 60_000));
    node.retrust().await;
    let (status, body) = node.post("/v1/orders", session_order_at(&account, &session, 0, NOW + 3 * 60_000)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("session key expired")));
    let session_id = eip712::address(session.verifying_key());
    assert_eq!(node.get(&format!("/v1/accounts/{session_id}/nonce")).await["next_nonce"], 0);
    assert!(
        node.entries().iter().all(
            |entry| !matches!(entry, LogEntry::Command(c) if matches!(c.sequenced.cmd, Command::AccessOrder { .. }))
        ),
        "the expired key's order never reached the log"
    );
}

#[tokio::test]
async fn a_session_key_cannot_revoke_itself_or_another_key() {
    let node = Node::start("session-key-revoke-scope", true).await;
    let account = key(1);
    let session = key(7);
    let other = key(8);
    node.onboard(&account).await;
    for (nonce, key) in [(1, &session), (2, &other)] {
        let register = registration(&account, key, nonce, NOW + 3_600_000);
        assert_eq!(node.post("/v1/session-keys", register).await.0, StatusCode::OK);
    }
    // The key's signature on a revocation is not the account's: refused before sequencing, the
    // registration stands, and no nonce is spent.
    let hash = eip712::session_key_revocation_hash(
        &eip712::domain(31_337, VAULT),
        eip712::address(account.verifying_key()),
        eip712::address(other.verifying_key()),
        0,
        NOW + 60_000,
    );
    let auth = RequestAuth { nonce: 0, expires_at: NOW + 60_000, signature: eip712::sign(&session, &hash) };
    let body = serde_json::json!({
        "account": eip712::address(account.verifying_key()),
        "session_key": eip712::address(other.verifying_key()),
        "auth": auth,
    });
    let (status, body) = delete(&node, "/v1/session-keys", body).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid signature")));
    assert!(
        node.entries().iter().all(|entry| !matches!(entry, LogEntry::Command(c) if matches!(c.sequenced.cmd, Command::RevokeSessionKey { .. }))),
        "the revocation never reached the log"
    );
    let session_id = eip712::address(session.verifying_key());
    assert_eq!(node.get(&format!("/v1/accounts/{session_id}/nonce")).await["next_nonce"], 0);
    // Nor can the key revoke itself.
    let hash = eip712::session_key_revocation_hash(
        &eip712::domain(31_337, VAULT),
        eip712::address(account.verifying_key()),
        session_id,
        1,
        NOW + 60_000,
    );
    let auth = RequestAuth { nonce: 1, expires_at: NOW + 60_000, signature: eip712::sign(&session, &hash) };
    let body = serde_json::json!({ "account": eip712::address(account.verifying_key()), "session_key": session_id, "auth": auth });
    let (status, body) = delete(&node, "/v1/session-keys", body).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid signature")));
    assert_eq!(node.get(&format!("/v1/accounts/{session_id}/nonce")).await["next_nonce"], 0);
    // Both registrations stand: either key still trades.
    let (status, events) = node.post("/v1/orders", session_order(&account, &other, 0)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "fill"), "{events}");
    let (status, events) = node.post("/v1/orders", session_order(&account, &session, 0)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "fill"), "{events}");
}

/// `account`'s signed registration of `session` until `session_expires_at`, as a JSON body.
pub(super) fn registration(account: &SigningKey, session: &SigningKey, nonce: u64, session_expires_at: i64) -> Value {
    json!({
        "account": eip712::address(account.verifying_key()),
        "session_key": eip712::address(session.verifying_key()),
        "expires_at": session_expires_at,
        "auth": signed_registration_auth(account, session, nonce, session_expires_at, account),
    })
}

/// The `signer`'s signature over registering `session` for `account`, as a JSON auth.
fn signed_registration_auth(
    account: &SigningKey,
    session: &SigningKey,
    nonce: u64,
    session_expires_at: i64,
    signer: &SigningKey,
) -> Value {
    let hash = eip712::session_key_registration_hash(
        &eip712::domain(31_337, VAULT),
        eip712::address(account.verifying_key()),
        eip712::address(session.verifying_key()),
        session_expires_at,
        nonce,
        NOW + 60_000,
    );
    json!(RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(signer, &hash) })
}

/// `account`'s signed revocation of `session`, as a JSON body.
pub(super) fn revocation(account: &SigningKey, session: &SigningKey, nonce: u64) -> Value {
    let hash = eip712::session_key_revocation_hash(
        &eip712::domain(31_337, VAULT),
        eip712::address(account.verifying_key()),
        eip712::address(session.verifying_key()),
        nonce,
        NOW + 60_000,
    );
    json!({
        "account": eip712::address(account.verifying_key()),
        "session_key": eip712::address(session.verifying_key()),
        "auth": RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(account, &hash) },
    })
}

/// A BTC market buy for `account`, signed by its session `key` with `nonce`, as a JSON body.
pub(super) fn session_order(account: &SigningKey, key: &SigningKey, nonce: u64) -> Value {
    session_order_at(account, key, nonce, NOW + 60_000)
}

/// `session_order` expiring at `expires_at`.
fn session_order_at(account: &SigningKey, key: &SigningKey, nonce: u64, expires_at: i64) -> Value {
    let mut request = OrderRequest {
        account: eip712::address(account.verifying_key()),
        client_id: 1,
        market: 1,
        side: Side::Buy,
        qty: "0.01".parse().unwrap(),
        kind: OrderKind::Market { quoted: Fixed::from_int(100000), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    };
    let hash = eip712::order_hash(&eip712::domain(31_337, VAULT), &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(key, &hash) });
    json!(request)
}

/// A withdrawal for `account`, signed by its session `key` with `nonce`, as a JSON body.
fn session_withdrawal(account: &SigningKey, key: &SigningKey, nonce: u64) -> Value {
    let amount = Fixed::from_int(10);
    let hash = eip712::withdrawal_hash(
        &eip712::domain(31_337, VAULT),
        eip712::address(account.verifying_key()),
        amount,
        nonce,
        NOW + 60_000,
    );
    json!(WithdrawalRequest {
        account: eip712::address(account.verifying_key()),
        amount,
        auth: Some(RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(key, &hash) })
    })
}

/// The status and JSON body of a DELETE of `path` with a JSON `body`.
pub(super) async fn delete(node: &Node, path: &str, body: Value) -> (StatusCode, Value) {
    let request =
        Request::delete(path).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
    let response = node.app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({ "error": String::from_utf8_lossy(&bytes) })))
}
