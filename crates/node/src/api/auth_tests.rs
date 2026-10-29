//! V19 API tests for signed client requests: signatures, nonces, expiries, and the account trust path.

use super::*;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::connect_info::MockConnectInfo;
use axum::http::Request as HttpRequest;
use engine_core::auth::MAX_EXPIRY_AHEAD_MS;
use engine_core::auth::RequestAuth;
use engine_core::command::{CancelRequest, Command, OrderKind, OrderRequest, WithdrawalRequest};
use engine_core::eip712;
use engine_core::eip712::{Eip712Domain, cancel_hash, order_hash, withdrawal_hash};
use engine_core::event::{Event, Reject};
use engine_core::price::SourceQuote;
use engine_core::types::{Millis, Side};
use governor::Quota;
use governor::clock::DefaultClock;
use k256::ecdsa::SigningKey;
use serde_json::Value;
use tower::ServiceExt;

use crate::clock::test_clock::TestClock;
use crate::config::RateLimit;
use crate::history::INDEX_FILE;
use crate::log::LogEntry;
use crate::sequencer::{Sequencer, SequencerConfig, SignedWithdrawal, replay};
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{dev_config, log_entries, sample_around, scratch_dir, sequencer_config};

const VECTORS: &str = include_str!("../../../core/tests/fixtures/eip712.json");
/// The time the test sequencer stamps every command with, and the API checks expiries against
/// unless a test gives it another clock: Tue 20 Oct 2026 14:00 UTC.
const NOW: Millis = 1_792_504_800_000;
const CHAIN_ID: u64 = 31_337;
const VAULT: AccountId = AccountId([0xaa; 20]);

#[tokio::test]
async fn a_signed_order_carries_the_listed_origin_and_no_other() {
    let mut node = Node::start("origin", signed()).await;
    let client = SigningKey::from_slice(&[1; 32]).unwrap();
    node.fund(&client).await;
    node.allow_origin("http://localhost:3000");
    // The origin each request sends, and the one the response grants.
    let listed = "http://localhost:3000";
    let cases = [(Some(listed), Some(listed)), (Some("http://evil.test"), None), (None, None)];
    for (nonce, (origin, granted)) in (0..).zip(cases) {
        let mut request = HttpRequest::post("/v1/orders").header("content-type", "application/json");
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        let body = signed_order(&client, nonce, "0.1").to_string();
        let response = node.app.clone().oneshot(request.body(Body::from(body)).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{origin:?}");
        let allowed = response.headers().get("access-control-allow-origin").map(|v| v.to_str().unwrap());
        assert_eq!(allowed, granted, "{origin:?}");
        let events: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert!(events.as_array().unwrap().iter().any(|e| e["type"] == "fill"), "{origin:?}: {events}");
    }
}

#[tokio::test]
async fn a_signed_order_fills_and_a_reused_or_ahead_nonce_stays_out_of_the_log() {
    let node = Node::start("stale", signed()).await;
    let client = SigningKey::from_slice(&[1; 32]).unwrap();
    node.fund(&client).await;

    let order = signed_order(&client, 0, "0.1");
    let (status, events) = node.post("/v1/orders", order.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(events.as_array().unwrap().iter().any(|e| e["type"] == "fill"), "{events}");

    let invalid_nonce = (StatusCode::CONFLICT, serde_json::json!({ "error": "invalid nonce", "expected": 1 }));
    assert_eq!(node.post("/v1/orders", order.clone()).await, invalid_nonce);
    assert_eq!(node.post("/v1/orders", signed_order(&client, 5, "0.1")).await, invalid_nonce);

    let mut altered = signed_order(&client, 1, "0.1");
    altered["qty"] = "0.2".into();
    let (status, body) = node.post("/v1/orders", altered).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid signature")));

    // Only the first order reached the log, and the log replays under the rules it recorded.
    let entries = node.entries();
    let orders = entries
        .iter()
        .filter(|e| matches!(e, LogEntry::Command(c) if matches!(c.sequenced.cmd, Command::PlaceOrder(_))));
    assert_eq!(orders.count(), 1);
    assert_eq!(replay(&entries).unwrap().engine.client_auth(), Some(&signed()));
}

#[tokio::test]
async fn only_a_request_the_node_would_sequence_spends_the_accounts_rate() {
    let node = Node::start("account-rate", signed()).await;
    let client = SigningKey::from_slice(&[1; 32]).unwrap();
    let forger = SigningKey::from_slice(&[2; 32]).unwrap();
    node.fund(&client).await;
    let burst = dev_config().api.per_account.burst.get();
    let first = signed_order(&client, 0, "0.001");
    assert_eq!(node.post("/v1/orders", first.clone()).await.0, StatusCode::OK);

    // Anyone can send these: each names the account, and none would be sequenced. If one kind spent
    // the account's rate, the burst below would run out early.
    let mut forged = signed_order(&forger, 1, "0.001");
    forged["account"] = serde_json::to_value(account_of(&client)).unwrap();
    let refused = [
        ("signed by another key", forged, StatusCode::UNAUTHORIZED),
        ("a replay of a used nonce", first, StatusCode::CONFLICT),
        ("a nonce ahead", signed_order(&client, 9, "0.001"), StatusCode::CONFLICT),
        ("an expired request", signed_order_until(&client, 1, "0.001", NOW - 1), StatusCode::BAD_REQUEST),
    ];
    for (what, request, status) in refused {
        for _ in 0..6 {
            assert_eq!(node.post("/v1/orders", request.clone()).await.0, status, "{what}");
        }
    }

    // The account's own sequenced requests spend it: the one above and the rest of the burst.
    for nonce in 1..u64::from(burst) {
        assert_eq!(node.post("/v1/orders", signed_order(&client, nonce, "0.001")).await.0, StatusCode::OK);
    }
    let over = signed_order(&client, u64::from(burst), "0.001");
    assert_eq!(node.post("/v1/orders", over).await.0, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn an_unsigned_request_is_refused_before_it_is_sequenced() {
    let node = Node::start("unsigned", signed()).await;
    let client = SigningKey::from_slice(&[1; 32]).unwrap();
    node.fund(&client).await;
    let account = account_of(&client);
    let mut order = signed_order(&client, 0, "0.1");
    order.as_object_mut().unwrap().remove("auth");
    let requests = [
        ("/v1/orders", order),
        ("/v1/orders/cancel", serde_json::json!({ "account": account, "order_id": 0 })),
        ("/v1/withdrawals", serde_json::json!({ "account": account, "amount": "100" })),
    ];
    let logged = node.entries().len();
    for (path, body) in requests {
        let (status, body) = node.post(path, body).await;
        assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("missing signature")), "{path}");
    }
    assert_eq!(node.entries().len(), logged);
}

#[tokio::test]
async fn a_signed_request_from_an_account_never_credited_stays_out_of_the_log() {
    let node = Node::start("uncredited", signed()).await;
    let stranger = SigningKey::from_slice(&[9; 32]).unwrap();
    let logged = node.entries().len();
    let (status, body) = node.post("/v1/orders", signed_order(&stranger, 0, "0.1")).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some("account has never been credited")));
    assert_eq!(node.entries().len(), logged);
}

#[tokio::test]
async fn an_expired_request_stays_out_of_the_log_and_a_too_far_one_uses_its_nonce() {
    let node = Node::start("expiry", signed()).await;
    let client = SigningKey::from_slice(&[1; 32]).unwrap();
    node.fund(&client).await;
    let logged = node.entries().len();
    let (status, body) = node.post("/v1/orders", signed_order_until(&client, 0, "0.1", NOW - 1)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().starts_with("request expired"), "{body}");
    assert_eq!(node.entries().len(), logged);

    // Refused by the engine, which uses the nonce, so the request can never run once its expiry
    // comes within the bound.
    let too_far = signed_order_until(&client, 0, "0.1", NOW + MAX_EXPIRY_AHEAD_MS + 1);
    let (status, events) = node.post("/v1/orders", too_far.clone()).await;
    assert_eq!((status, events[0]["reason"]["kind"].as_str()), (StatusCode::OK, Some("expiry_too_far")), "{events}");
    let invalid_nonce = (StatusCode::CONFLICT, serde_json::json!({ "error": "invalid nonce", "expected": 1 }));
    assert_eq!(node.post("/v1/orders", too_far).await, invalid_nonce);
}

#[tokio::test]
async fn a_request_expired_by_the_clock_stays_out_of_the_log_though_sequenced_time_lags() {
    // The API's clock runs a minute and a millisecond past the last sequenced time.
    let node = Node::start_with_clock("clock", signed(), || NOW + 60_001).await;
    let client = SigningKey::from_slice(&[1; 32]).unwrap();
    node.fund(&client).await;
    let logged = node.entries().len();
    let (status, body) = node.post("/v1/orders", signed_order(&client, 0, "0.1")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().starts_with("request expired"), "{body}");
    assert_eq!(node.entries().len(), logged);
}

#[tokio::test]
async fn a_request_expired_by_sequenced_time_stays_out_of_the_log_though_the_clock_lags() {
    // The API's clock reads the epoch, far behind sequenced time.
    let node = Node::start_with_clock("lagging-clock", signed(), || 0).await;
    let client = SigningKey::from_slice(&[1; 32]).unwrap();
    node.fund(&client).await;
    let last_sequenced = node.state.query(|e| e.now()).await.expect("the sequencer is running");
    let logged = node.entries().len();
    let (status, body) = node.post("/v1/orders", signed_order_until(&client, 0, "0.1", last_sequenced - 1)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().starts_with("request expired"), "{body}");
    assert_eq!(node.entries().len(), logged);
}

#[tokio::test]
async fn two_copies_of_a_withdrawal_that_both_pass_the_api_are_paid_once() {
    let mut node = Node::start("race", signed()).await;
    let client = SigningKey::from_slice(&[1; 32]).unwrap();
    node.fund(&client).await;
    let (account, amount, expires_at) = (account_of(&client), "100".parse::<Fixed>().unwrap(), NOW + 60_000);
    let hash = |domain: &Eip712Domain, auth: &RequestAuth| {
        withdrawal_hash(domain, account, amount, auth.nonce, auth.expires_at)
    };
    let signature =
        eip712::sign(&client, &withdrawal_hash(&eip712::domain(CHAIN_ID, VAULT), account, amount, 0, expires_at));
    let auth = RequestAuth { nonce: 0, expires_at, signature };
    // Both copies pass the API's checks before either is sequenced, as two racing requests can.
    let first = node.state.authorise(account, Some(auth.clone()), hash).await.expect("passes the API");
    let second = node.state.authorise(account, Some(auth), hash).await.expect("passes the API");
    let withdraw = |auth| Command::Withdraw { account, amount, auth };
    let approved = node.state.submit(withdraw(first)).await.expect("the sequencer is running");
    let refused = node.state.submit(withdraw(second)).await.expect("the sequencer is running");
    assert_eq!(
        approved.iter().filter(|event| matches!(event, Event::WithdrawalApproved { .. })).count(),
        1,
        "{approved:?}"
    );
    let expected = Reject::InvalidNonce { expected: 1 };
    assert!(matches!(&refused[..], [Event::WithdrawalRejected { reason, .. }] if *reason == expected), "{refused:?}");
    assert!(node.withdrawals.try_recv().is_ok());
    assert!(node.withdrawals.try_recv().is_err(), "a second payout was signed");
}

#[tokio::test]
async fn signed_cancels_and_withdrawals_reach_the_engine_and_advance_the_nonce() {
    let node = Node::start("cancel", signed()).await;
    let client = SigningKey::from_slice(&[1; 32]).unwrap();
    node.fund(&client).await;
    let (account, domain) = (account_of(&client), eip712::domain(CHAIN_ID, VAULT));

    let mut limit: OrderRequest = serde_json::from_value(signed_order(&client, 0, "0.1")).unwrap();
    limit.kind = OrderKind::Limit { price: "90000".parse().unwrap() };
    let expires_at = NOW + 60_000;
    let signature = eip712::sign(&client, &order_hash(&domain, &limit, 0, expires_at));
    limit.auth = Some(RequestAuth { nonce: 0, expires_at, signature });
    let (_, events) = node.post("/v1/orders", serde_json::to_value(&limit).unwrap()).await;
    let order_id = events[0]["order_id"].as_u64().unwrap();

    let signature = eip712::sign(&client, &cancel_hash(&domain, account, order_id, 1, expires_at));
    let auth = RequestAuth { nonce: 1, expires_at, signature };
    let cancel = serde_json::json!({ "account": account, "order_id": order_id, "auth": auth });
    let (_, events) = node.post("/v1/orders/cancel", cancel).await;
    assert_eq!(events[0]["type"], "order_cancelled", "{events}");

    let amount: Fixed = "100".parse().unwrap();
    let signature = eip712::sign(&client, &withdrawal_hash(&domain, account, amount, 2, expires_at));
    let auth = RequestAuth { nonce: 2, expires_at, signature };
    let withdrawal = serde_json::json!({ "account": account, "amount": amount, "auth": auth });
    let (_, events) = node.post("/v1/withdrawals", withdrawal).await;
    let kinds: Vec<_> = events.as_array().unwrap().iter().map(|event| event["type"].clone()).collect();
    assert_eq!(kinds, ["withdrawal_queued", "withdrawal_approved", "withdrawal_queue_updated"], "{events}");

    let (status, nonce) = node.get(&format!("/v1/accounts/{account}/nonce")).await;
    assert_eq!((status, nonce), (StatusCode::OK, serde_json::json!({ "next_nonce": 3 })));
}

#[tokio::test]
async fn without_signatures_the_account_field_is_trusted() {
    let node = Node::start("trusted", ClientAuth::Unsigned).await;
    let client = SigningKey::from_slice(&[1; 32]).unwrap();
    node.fund(&client).await;
    // A signature, valid or not, is dropped rather than sequenced.
    let mut order = signed_order(&client, 7, "0.1");
    order["qty"] = "0.2".into();
    let (status, events) = node.post("/v1/orders", order).await;
    assert_eq!(status, StatusCode::OK);
    assert!(events.as_array().unwrap().iter().any(|e| e["type"] == "fill"), "{events}");
    let sequenced = node.entries().into_iter().find_map(|e| match e {
        LogEntry::Command(c) => match c.sequenced.cmd {
            Command::PlaceOrder(req) => Some(req),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(sequenced.map(|req| req.auth), Some(None));
}

#[test]
fn the_published_signing_vectors_are_bodies_the_api_accepts() {
    let vectors: Value = serde_json::from_str(VECTORS).unwrap();
    for vector in vectors["requests"].as_array().unwrap() {
        let mut body = vector["body"].clone();
        let (nonce, expires_at, signature) = (&vector["nonce"], &vector["expires_at"], &vector["signature"]);
        body["auth"] = serde_json::json!({ "nonce": nonce, "expires_at": expires_at, "signature": signature });
        let parsed = match vector["kind"].as_str().unwrap() {
            "order" => serde_json::from_value::<OrderRequest>(body).map(drop),
            "cancel" => serde_json::from_value::<CancelRequest>(body).map(drop),
            "withdrawal" => serde_json::from_value::<WithdrawalRequest>(body).map(drop),
            other => panic!("unknown request kind {other}"),
        };
        assert!(parsed.is_ok(), "{}: {parsed:?}", vector["kind"]);
    }
}

#[tokio::test]
async fn a_nonce_conflict_is_signed_over_the_order_it_answers_so_no_one_between_can_invent_one() {
    let node = Node::start("signed-conflict", signed()).await;
    let client = SigningKey::from_slice(&[1; 32]).unwrap();
    node.fund(&client).await;
    let challenge = "1b6d3f0a8c2e4975b1d0e6a3f8c27b4e9d05a1c3e7f2b6d48a0c9e1f3b5d7a26";
    let order = signed_order(&client, 5, "0.1").to_string();
    let request = HttpRequest::post("/v1/orders")
        .header("content-type", "application/json")
        .header(engine_wire::CHALLENGE_HEADER, challenge)
        .body(Body::from(order.clone()));
    let response = node.app.clone().oneshot(request.unwrap()).await.unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT);
    let challenge = super::signing_tests::parse(challenge);
    let verify = |sent: &str| {
        super::signing_tests::verify(("POST", "/v1/orders", sent.as_bytes()), challenge, status, &headers, &body)
    };
    assert_eq!(verify(&order), Ok(()));
    // The answer does not verify for another account's order swapped in under the same challenge.
    let other = signed_order(&SigningKey::from_slice(&[2; 32]).unwrap(), 5, "0.1").to_string();
    assert_eq!(verify(&other), Err(engine_wire::Error::WrongSigner));
}

// ── Helpers ──

/// A node API over a running sequencer on a fresh log, with the dev genesis and `auth` recorded.
struct Node {
    app: Router,
    state: ApiState,
    dir: PathBuf,
    withdrawals: mpsc::UnboundedReceiver<SignedWithdrawal>,
}

impl Node {
    /// Recovers a sequencer on a fresh log named after `name`, activates the protocol, sequences
    /// the dev genesis, and serves the API over it.
    async fn start(name: &str, auth: ClientAuth) -> Self {
        Node::start_with_clock(name, auth, || NOW).await
    }

    /// `start`, with the API checking expiries against `clock` while the sequencer stamps `NOW`.
    async fn start_with_clock(name: &str, auth: ClientAuth, clock: fn() -> Millis) -> Self {
        let dir = scratch_dir(&format!("auth-{name}"));
        let key = Arc::new(EnclaveKey::generate());
        let (stream, _) = broadcast::channel(1024);
        let (withdrawals_tx, withdrawals) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let stamps = Arc::new(TestClock::at(NOW));
        let cfg = SequencerConfig {
            chain_id: CHAIN_ID,
            vault: VAULT,
            operator: engine_core::auth::Operator {
                chain_id: CHAIN_ID,
                vault: VAULT,
                ..dev_config().operator().unwrap()
            },
            clock: stamps.clone(),
            client_auth: auth.clone(),
            ..sequencer_config(&dir, manifest_tx)
        };
        let mut seq = Sequencer::recover(cfg, key.clone(), stream.clone(), withdrawals_tx).unwrap();
        seq.activate_legacy_protocol().unwrap();
        seq.genesis(&dev_config().genesis).unwrap();
        // Trusted time agrees with the fixed clock, so the queued activation and genesis go in now.
        seq.trust(sample_around(stamps.as_ref())).unwrap();
        let (tx, rx) = mpsc::channel(1024);
        tokio::spawn(seq.run(rx));
        let attestation = Arc::new(
            attest_bundle(
                &MockAttestor,
                &AttestationBundle {
                    enclave_address: key.address(),
                    config_sha256: [0; 32],
                    tls_public_key_sha256: [0; 32],
                },
            )
            .unwrap(),
        );
        // The dev limits, on buckets that refill once an hour, so no test depends on its speed.
        let hourly = |rate: RateLimit| Quota::with_period(Duration::from_secs(3600)).unwrap().allow_burst(rate.burst);
        let state = ApiState {
            protocol: engine_core::protocol::ProtocolVersion::V23,
            access: None,
            tx,
            stream,
            attestation,
            auth,
            operator: dev_config().operator().unwrap(),
            mock_custody: true,
            payouts_enabled: true,
            clock,
            log_dir: dir.clone(),
            manifest,
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            limits: Arc::new(ApiLimits::with_quota(&dev_config().api, hourly, DefaultClock::default())),
            history: Reader::new(dir.join(INDEX_FILE), watch::channel(true).1),
            emission_budget: None,
            collateral: None,
            listing: Default::default(),
            signing: crate::test_support::signing(),
        };
        let app = router(state.clone()).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        Node { app, state, dir, withdrawals }
    }

    /// Allows browsers at `origin` to call the API, which serves them from here on.
    fn allow_origin(&mut self, origin: &str) {
        let mut api = dev_config().api;
        api.allowed_origins = vec![origin.to_owned()];
        let hourly = |rate: RateLimit| Quota::with_period(Duration::from_secs(3600)).unwrap().allow_burst(rate.burst);
        self.state.limits = Arc::new(ApiLimits::with_quota(&api, hourly, DefaultClock::default()));
        self.app = router(self.state.clone()).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
    }

    /// Every entry of the node's log, as flushed so far.
    fn entries(&self) -> Vec<LogEntry> {
        log_entries(&self.dir)
    }

    /// Credits `client`'s account through the dev deposit and prices BTC at 100000.
    async fn fund(&self, client: &SigningKey) {
        let deposit = serde_json::json!({ "account": account_of(client), "amount": "100000" });
        assert_eq!(self.post("/v1/dev/deposit", deposit).await.0, StatusCode::OK);
        let quotes = ["a", "b"]
            .map(|venue| SourceQuote { source: venue.into(), mid: "100000".parse().unwrap(), ts: NOW, contract: None })
            .to_vec();
        self.state.submit(Command::Prices { market: 1, quotes }).await.expect("the sequencer is running");
    }

    /// The status and JSON body of a POST of `body` to `path`.
    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let request =
            HttpRequest::post(path).header("content-type", "application/json").body(Body::from(body.to_string()));
        self.send(request.unwrap()).await
    }

    /// The status and JSON body of a GET of `path`.
    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(HttpRequest::get(path).body(Body::empty()).unwrap()).await
    }

    /// Serves one request and reads its JSON response.
    async fn send(&self, request: HttpRequest<Body>) -> (StatusCode, Value) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
}

/// Signed requests under the test chain and vault.
fn signed() -> ClientAuth {
    ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }
}

/// The account `key` controls.
fn account_of(key: &SigningKey) -> AccountId {
    eip712::address(key.verifying_key())
}

/// A BTC market buy of `qty` quoted at 100000, signed by `key` with `nonce` to expire in a minute,
/// as a JSON body.
fn signed_order(key: &SigningKey, nonce: u64, qty: &str) -> Value {
    signed_order_until(key, nonce, qty, NOW + 60_000)
}

/// `signed_order` expiring at `expires_at`.
fn signed_order_until(key: &SigningKey, nonce: u64, qty: &str, expires_at: Millis) -> Value {
    let mut req = OrderRequest {
        account: account_of(key),
        client_id: 1,
        market: 1,
        side: Side::Buy,
        qty: qty.parse().unwrap(),
        kind: OrderKind::Market { quoted: "100000".parse().unwrap(), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    };
    let hash = order_hash(&eip712::domain(CHAIN_ID, VAULT), &req, nonce, expires_at);
    req.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(key, &hash) });
    serde_json::to_value(&req).unwrap()
}
