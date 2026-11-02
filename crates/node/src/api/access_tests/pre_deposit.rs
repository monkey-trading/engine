//! A wallet sets up before its first deposit: it declares, then manages its session keys, while
//! everything else it or its keys sign stays refused until a deposit credits it.

use engine_core::session_keys::{MAX_SESSION_KEY_EXPIRY_MS, MAX_SESSION_KEYS_PER_ACCOUNT, SessionKey};

use super::admission::settled_entries;
use super::batch::{cancel_all, close_all};
use super::session_keys::{delete, registration, revocation, session_order, session_withdrawal};
use super::*;
use crate::custody::pending::retry_batch;
use crate::exit::{ExitSubject, exit_bundle};

const NEVER_CREDITED: &str = "account has never been credited";
const MUST_DECLARE: &str = "a never-credited account must declare first";
pub(super) const HOUR_MS: i64 = 3_600_000;

#[tokio::test]
async fn a_fresh_wallet_declares_then_registers_and_stays_out_of_trading_until_credited() {
    let node = Node::start("pre-deposit-order", true).await;
    let (wallet, session) = (key(1), key(7));
    let (account, session_id) = (address(&wallet), address(&session));
    let (entries, queries) = (settled_entries(&node).await, screened(&node));

    // Before it declares, nothing it signs reaches the source or the log, or uses its nonce.
    let (status, body) = node.post("/v1/orders", order(&wallet, 0)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some(NEVER_CREDITED)));
    let (status, body) = node.post("/v1/session-keys", registration(&wallet, &session, 0, NOW + HOUR_MS)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some(MUST_DECLARE)));
    let (status, body) = delete(&node, "/v1/session-keys", revocation(&wallet, &session, 0)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some(MUST_DECLARE)));
    assert_eq!(settled_entries(&node).await, entries);
    assert_eq!(screened(&node), queries);
    assert_eq!(next_nonce(&node, account).await, 0);

    // The declaration records with no deposit, and is the one request screened.
    let (status, events) = node.post("/v1/eligibility", declaration(&wallet, 0)).await;
    assert_eq!((status, &events[0]["type"]), (StatusCode::OK, &json!("eligibility_attested")), "{events}");
    assert_eq!(screened(&node), queries + 1);
    // A second declaration is refused before the log, at its next nonce or at the spent one.
    let (status, body) = node.post("/v1/eligibility", declaration(&wallet, 1)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some("account has already declared")));
    assert_eq!(node.post("/v1/eligibility", declaration(&wallet, 0)).await.0, StatusCode::CONFLICT);
    assert_eq!(settled_entries(&node).await, entries + 1);
    assert_eq!(screened(&node), queries + 1);

    // The registration then applies; the key's sequence is its own and has not moved.
    let (status, events) = node.post("/v1/session-keys", registration(&wallet, &session, 1, NOW + HOUR_MS)).await;
    assert_eq!((status, &events[0]["type"]), (StatusCode::OK, &json!("session_key_registered")), "{events}");
    assert_eq!(screened(&node), queries + 1, "the registration relies on the declaration's screening");
    let (nonces, logged) = ((2, 0), entries + 2);
    assert_eq!((next_nonce(&node, account).await, next_nonce(&node, session_id).await), nonces);

    // Everything else stays refused, whichever key signs it, and uses neither sequence.
    let cancel = |signer: &SigningKey, nonce| {
        let hash = eip712::cancel_hash(&eip712::domain(31_337, VAULT), account, 1, nonce, NOW + 60_000);
        let auth = RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(signer, &hash) };
        json!({ "account": account, "order_id": 1, "auth": auth })
    };
    for (signer, nonce) in [(&wallet, 2), (&session, 0)] {
        let requests = [
            ("/v1/orders", if nonce == 2 { order(&wallet, 2) } else { session_order(&wallet, &session, 0) }),
            ("/v1/orders/cancel", cancel(signer, nonce)),
            ("/v1/orders/cancel-all", cancel_all(&wallet, signer, nonce)),
            ("/v1/positions/close-all", close_all(&wallet, signer, nonce, &[1], 100000)),
            (
                "/v1/withdrawals",
                if nonce == 2 { withdrawal(&wallet, 2) } else { session_withdrawal(&wallet, &session, 0) },
            ),
        ];
        for (path, request) in requests {
            let (status, body) = node.post(path, request).await;
            assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some(NEVER_CREDITED)), "{path}");
        }
    }
    assert_eq!((next_nonce(&node, account).await, next_nonce(&node, session_id).await), nonces);
    assert_eq!(settled_entries(&node).await, logged);
    assert_eq!(screened(&node), queries + 1);

    // A session key cannot stand in for the account on its declaration or its keys.
    let stranger = key(8);
    let mut forged = registration(&wallet, &stranger, 2, NOW + HOUR_MS);
    forged["auth"] = key_management_auth(&session, registration_hash(&wallet, &stranger, 2, NOW + 60_000), 2);
    let (status, body) = node.post("/v1/session-keys", forged).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid signature")));
    let mut forged = revocation(&wallet, &stranger, 2);
    forged["auth"] = key_management_auth(&session, revocation_hash(&wallet, &stranger, 2, NOW + 60_000), 2);
    let (status, body) = delete(&node, "/v1/session-keys", forged).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid signature")));
    assert_eq!(settled_entries(&node).await, logged);

    // The wallet revokes the key it registered, and a revoked key is refused like a live one.
    let (status, events) = delete(&node, "/v1/session-keys", revocation(&wallet, &session, 2)).await;
    assert_eq!((status, &events[0]["type"]), (StatusCode::OK, &json!("session_key_revoked")), "{events}");
    let (status, body) = node.post("/v1/orders", session_order(&wallet, &session, 0)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some(NEVER_CREDITED)));

    // The log replays to the same account state, with no account entry.
    let replayed = replay(&node.entries()).unwrap().engine;
    assert_eq!(replayed.eligibility(account), Some(EligibilityClass::Professional));
    assert_eq!(
        replayed.session_key(account, session_id),
        Some(SessionKey { expires_at: NOW + HOUR_MS, revoked: true }),
        "the replay holds the revoked registration"
    );
    assert_eq!((replayed.next_request_nonce(account), replayed.next_request_nonce(session_id)), (3, 0));
    assert!(!replayed.accounts().contains_key(&account));
}

#[tokio::test]
async fn a_refused_declaration_leaves_the_key_routes_closed_until_one_is_recorded() {
    let node = Node::start("pre-deposit-refused", true).await;
    let (wallet, session) = (key(1), key(7));
    let account = address(&wallet);
    node.source.lock().unwrap().jurisdiction = Jurisdiction::UnitedStates;
    let (status, events) = node.post("/v1/eligibility", declaration(&wallet, 0)).await;
    assert_eq!((status, &events[0]["type"]), (StatusCode::OK, &json!("eligibility_rejected")), "{events}");
    assert_eq!(next_nonce(&node, account).await, 1, "the refusal used its nonce");
    let entries = settled_entries(&node).await;

    // The spent nonce does not stand in for a recorded declaration, on either route.
    let (status, body) = node.post("/v1/session-keys", registration(&wallet, &session, 1, NOW + HOUR_MS)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some(MUST_DECLARE)));
    let (status, body) = delete(&node, "/v1/session-keys", revocation(&wallet, &session, 1)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some(MUST_DECLARE)));
    assert_eq!(settled_entries(&node).await, entries);
    assert_eq!(next_nonce(&node, account).await, 1);

    // An allowed declaration opens them.
    node.source.lock().unwrap().jurisdiction = Jurisdiction::Allowed;
    let (status, events) = node.post("/v1/eligibility", declaration(&wallet, 1)).await;
    assert_eq!((status, &events[0]["type"]), (StatusCode::OK, &json!("eligibility_attested")), "{events}");
    assert_eq!(
        node.post("/v1/session-keys", registration(&wallet, &session, 2, NOW + HOUR_MS)).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_declaration_while_activation_waits_for_trusted_time_never_reaches_the_log() {
    let node = Node::start_with_time("pre-deposit-startup", true, false).await;
    let (status, body) = node.post("/v1/eligibility", declaration(&key(1), 0)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("engine unavailable")));
    assert!(node.entries().is_empty());
}

#[tokio::test]
async fn a_deposit_after_the_declaration_credits_at_its_first_screening_and_the_key_then_trades() {
    let node = Node::start("pre-deposit-credit", true).await;
    let (wallet, session) = (key(1), key(7));
    let (account, session_id) = (address(&wallet), address(&session));
    assert_eq!(node.post("/v1/eligibility", declaration(&wallet, 0)).await.0, StatusCode::OK);
    assert_eq!(
        node.post("/v1/session-keys", registration(&wallet, &session, 1, NOW + HOUR_MS)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        node.state.query(|e| e.retryable_pending("", 8).len()).await.unwrap(),
        0,
        "no retry work for a free wallet"
    );

    let events = deposit(&node, account, "0xfunding:first").await;
    assert_eq!(events, [Event::Deposited { account, amount: Fixed::from_int(100000) }]);
    assert_eq!(node.state.query(|e| e.pending_deposits().len()).await.unwrap(), 0);

    // No further account signature: the key the wallet registered before depositing fills an order.
    let (status, events) = node.post("/v1/orders", session_order(&wallet, &session, 0)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "fill"), "{events}");
    assert_eq!((next_nonce(&node, account).await, next_nonce(&node, session_id).await), (2, 1));
    let replayed = replay(&node.entries()).unwrap().engine;
    assert_eq!((replayed.next_request_nonce(account), replayed.next_request_nonce(session_id)), (2, 1));
    assert!(replayed.accounts()[&account].positions.contains_key(&1));
}

#[tokio::test]
async fn a_deposit_before_the_declaration_stays_pending_and_the_retry_worker_credits_it_later() {
    let node = Node::start("pre-deposit-pending", true).await;
    let (wallet, session) = (key(1), key(7));
    let account = address(&wallet);
    let pending = deposit(&node, account, "0xfunding:direct").await;
    assert!(matches!(pending.as_slice(), [Event::DepositPending { .. }]), "{pending:?}");

    // A pending deposit does not stand in for the declaration: its keys still wait for one.
    let (status, body) = node.post("/v1/session-keys", registration(&wallet, &session, 0, NOW + HOUR_MS)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some(MUST_DECLARE)));
    // The custody retry worker skips a deposit whose account has not declared.
    retry_turn(&node).await;
    assert_eq!(node.state.query(|e| e.pending_deposits().len()).await.unwrap(), 1);

    assert_eq!(node.post("/v1/eligibility", declaration(&wallet, 0)).await.0, StatusCode::OK);
    assert_eq!(
        node.post("/v1/session-keys", registration(&wallet, &session, 1, NOW + HOUR_MS)).await.0,
        StatusCode::OK
    );
    let (status, body) = node.post("/v1/orders", session_order(&wallet, &session, 0)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some(NEVER_CREDITED)), "not credited yet");

    retry_turn(&node).await;
    assert_eq!(node.state.query(move |e| e.accounts()[&account].balance).await.unwrap(), Fixed::from_int(100000));
    assert_eq!(node.state.query(|e| e.pending_deposits().len()).await.unwrap(), 0);
    let (status, events) = node.post("/v1/orders", session_order(&wallet, &session, 0)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "fill"), "{events}");
}

#[tokio::test]
async fn a_key_request_the_engine_refuses_is_public_uses_its_nonce_and_replays() {
    let node = Node::start("pre-deposit-engine-refusal", true).await;
    let (wallet, session, other) = (key(1), key(7), key(8));
    let account = address(&wallet);
    assert_eq!(node.post("/v1/eligibility", declaration(&wallet, 0)).await.0, StatusCode::OK);

    let reason = |events: &Value| {
        assert_eq!(events.as_array().unwrap().len(), 1, "{events}");
        assert_eq!(events[0]["type"], "session_key_rejected", "{events}");
        events[0]["reason"]["kind"].as_str().unwrap().to_owned()
    };
    let cases = [
        (registration(&wallet, &wallet, 1, NOW + HOUR_MS), "invalid_session_key"),
        (registration(&wallet, &session, 2, NOW), "expired"),
        (registration(&wallet, &session, 3, NOW + MAX_SESSION_KEY_EXPIRY_MS + 1), "expiry_too_far"),
    ];
    for (nonce, (request, expected)) in (1..).zip(cases) {
        let (status, events) = node.post("/v1/session-keys", request).await;
        assert_eq!((status, reason(&events)), (StatusCode::OK, expected.to_owned()));
        assert_eq!(next_nonce(&node, account).await, nonce + 1, "{expected} used its nonce");
    }
    // A revocation of a key the account never registered is refused the same way.
    let (status, events) = delete(&node, "/v1/session-keys", revocation(&wallet, &other, 4)).await;
    assert_eq!((status, reason(&events)), (StatusCode::OK, "invalid_session_key".to_owned()));
    assert_eq!(next_nonce(&node, account).await, 5);

    // The live-key cap binds a never-credited account as it does any other.
    let cap = MAX_SESSION_KEYS_PER_ACCOUNT as u8;
    for index in 0..cap {
        let request = registration(&wallet, &key(0x20 + index), 5 + u64::from(index), NOW + HOUR_MS);
        assert_eq!(node.post("/v1/session-keys", request).await.0, StatusCode::OK);
    }
    let over = registration(&wallet, &session, 5 + u64::from(cap), NOW + HOUR_MS);
    let (status, events) = node.post("/v1/session-keys", over).await;
    assert_eq!((status, reason(&events)), (StatusCode::OK, "too_many_session_keys".to_owned()));
    assert_eq!(events[0]["reason"]["max"], 10);

    let refusals = logged_refusals(&node);
    assert_eq!(refusals, 5, "every refusal is in the public log");
    let replayed = replay(&node.entries()).unwrap().engine;
    assert_eq!(replayed.next_request_nonce(account), 6 + u64::from(cap));
    assert_eq!(replayed.session_key_registrations(account), u64::from(cap));
    assert_eq!(replayed.session_key(account, address(&session)), None);
}

#[tokio::test]
async fn a_wallet_with_a_declaration_and_keys_but_no_account_meets_every_read_and_scan() {
    let node = Node::start("pre-deposit-reads", true).await;
    let (wallet, session) = (key(1), key(7));
    let account = address(&wallet);
    let unknown = address(&key(8));
    assert_eq!(node.post("/v1/eligibility", declaration(&wallet, 0)).await.0, StatusCode::OK);
    assert_eq!(
        node.post("/v1/session-keys", registration(&wallet, &session, 1, NOW + HOUR_MS)).await.0,
        StatusCode::OK
    );
    for _ in 0..3 {
        node.state.submit(Command::Tick).await.unwrap();
    }
    let checkpoint = settled_checkpoint(&node).await;

    // What a console reads for this wallet, against a wallet it has never seen.
    assert_eq!(status_of(&node, &format!("/v1/accounts/{account}")).await, StatusCode::NOT_FOUND);
    assert_eq!(node.get(&format!("/v1/accounts/{account}/nonce")).await["next_nonce"], 2);
    assert_eq!(node.get(&format!("/v1/accounts/{unknown}/nonce")).await["next_nonce"], 0);
    let declared = node.get(&format!("/v1/accounts/{account}/eligibility")).await;
    assert_eq!(declared["class"], "professional");
    assert_eq!(declared["statement"], "I attest that I am a professional client.");
    assert_eq!(node.get(&format!("/v1/accounts/{unknown}/eligibility")).await["class"], Value::Null);
    assert_eq!(status_of(&node, &format!("/v1/accounts/{account}/proof")).await, StatusCode::NOT_FOUND);
    assert_eq!(status_of(&node, &format!("/v1/accounts/{account}/emissions")).await, StatusCode::NOT_FOUND);
    assert_eq!(status_of(&node, &format!("/v1/accounts/{account}/distributions")).await, StatusCode::NOT_FOUND);
    assert_eq!(status_of(&node, &format!("/v1/accounts/{account}/withdrawals")).await, StatusCode::OK);
    assert_eq!(status_of(&node, &format!("/v1/accounts/{account}/payouts")).await, StatusCode::OK);

    // An exit has nothing to register for it: its proof holds only the exit terms.
    let exit: engine_core::merkle::ExitProof =
        serde_json::from_value(node.get(&format!("/v1/accounts/{account}/exit")).await).unwrap();
    assert!(exit.account.is_none() && exit.approvals.is_empty() && exit.queued.is_empty());
    assert!(exit.distribution.is_none() && exit.pending_deposits.is_empty());
    let bundle = exit_bundle(&node.dir, checkpoint, ExitSubject::Account(account)).unwrap();
    let [start] = &bundle.calls[..] else { panic!("only startExit, nothing to register: {:?}", bundle.calls) };
    assert!(start.what.starts_with("startExit"), "{}", start.what);

    // The dashboard's scan reads accounts only, so the wallet adds no position or margin.
    let stats = node.state.stats.clone();
    node.state.query(move |engine| stats.finish_recovery(engine)).await.unwrap();
    let house = node.get("/v1/house").await;
    assert_eq!((&house["open_positions"], &house["open_interest"]), (&json!(0), &json!("0")), "{house}");
}

// ── Helpers ──

pub(super) fn address(key: &SigningKey) -> AccountId {
    eip712::address(key.verifying_key())
}

/// How many requests the access source has answered.
pub(super) fn screened(node: &Node) -> usize {
    node.source.lock().unwrap().requests.len()
}

/// The nonce the node's route reports for `account`.
pub(super) async fn next_nonce(node: &Node, account: AccountId) -> u64 {
    node.get(&format!("/v1/accounts/{account}/nonce")).await["next_nonce"].as_u64().unwrap()
}

/// `account`'s own deposit of 100000 under `reference`, screened as custody screens it.
async fn deposit(node: &Node, account: AccountId, reference: &str) -> Vec<Event> {
    let reference = reference.to_owned();
    let command = node
        .state
        .query(move |engine| {
            crate::test_support::access::funding(
                engine,
                Some(account),
                Some(account),
                Fixed::from_int(100000),
                &reference,
            )
        })
        .await
        .unwrap();
    node.state.submit(command).await.unwrap()
}

/// One turn of the custody worker that retries pending deposits.
async fn retry_turn(node: &Node) {
    retry_batch(node.state.access.as_deref(), &node.state.tx, &mut None).await.unwrap();
}

/// The commands whose events hold a session-key refusal.
fn logged_refusals(node: &Node) -> usize {
    node.entries()
        .iter()
        .filter(|entry| {
            matches!(entry, LogEntry::Command(command)
                if command.events.as_ref().is_some_and(|events| events.iter().any(|event|
                    matches!(event, Event::SessionKeyRejected { .. }))))
        })
        .count()
}

/// The seq of the last checkpoint, once the writer has flushed everything applied.
async fn settled_checkpoint(node: &Node) -> u64 {
    settled_entries(node).await;
    node.entries()
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint.seq),
            _ => None,
        })
        .next_back()
        .expect("the ticks reach a checkpoint")
}

/// The status of a GET of `path`.
async fn status_of(node: &Node, path: &str) -> StatusCode {
    node.app.clone().oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap().status()
}

/// The digest of `account` registering `session` until `NOW + HOUR_MS`, as `registration` builds it.
pub(super) fn registration_hash(
    account: &SigningKey,
    session: &SigningKey,
    nonce: u64,
    expires_at: i64,
) -> eip712::B256 {
    eip712::session_key_registration_hash(
        &eip712::domain(31_337, VAULT),
        address(account),
        address(session),
        NOW + HOUR_MS,
        nonce,
        expires_at,
    )
}

/// The digest of `account` revoking `session`.
pub(super) fn revocation_hash(account: &SigningKey, session: &SigningKey, nonce: u64, expires_at: i64) -> eip712::B256 {
    eip712::session_key_revocation_hash(
        &eip712::domain(31_337, VAULT),
        address(account),
        address(session),
        nonce,
        expires_at,
    )
}

/// `signer`'s signature over `hash` with `nonce`, as a JSON auth expiring at the usual minute.
pub(super) fn key_management_auth(signer: &SigningKey, hash: eip712::B256, nonce: u64) -> Value {
    json!(RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(signer, &hash) })
}
