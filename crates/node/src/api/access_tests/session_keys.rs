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

/// `account`'s signed registration of `session` until `session_expires_at`, as a JSON body.
fn registration(account: &SigningKey, session: &SigningKey, nonce: u64, session_expires_at: i64) -> Value {
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
fn revocation(account: &SigningKey, session: &SigningKey, nonce: u64) -> Value {
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
fn session_order(account: &SigningKey, key: &SigningKey, nonce: u64) -> Value {
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
async fn delete(node: &Node, path: &str, body: Value) -> (StatusCode, Value) {
    let request =
        Request::delete(path).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
    let response = node.app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({ "error": String::from_utf8_lossy(&bytes) })))
}
