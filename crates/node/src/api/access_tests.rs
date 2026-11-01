//! Authenticated API refusals use the actual transport peer and are durable before reply.

mod admission;
mod declarations;
mod reissue;
mod session_keys;
mod source;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Extension, Json, Router};
use engine_core::access::{EligibilityClass, EligibilityRequest, Jurisdiction};
use engine_core::auth::{ClientAuth, Operator, RequestAuth};
use engine_core::command::{Command, OrderKind, OrderRequest, WithdrawalRequest};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};
use engine_core::{Engine, Fixed};
use governor::Quota;
use governor::clock::DefaultClock;
use k256::ecdsa::SigningKey;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tower::ServiceExt;

use crate::access::AccessClient;
use crate::api::{ApiState, MAX_BACKFILLS, router};
use crate::clock::test_clock::TestClock;
use crate::config::{AccessSection, RateLimit};
use crate::history::{INDEX_FILE, Reader};
use crate::limits::ApiLimits;
use crate::log::{LogEntry, Manifest};
use crate::sequencer::{self, Sequencer, SignedWithdrawal, replay, replay_with_signer};
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{dev_config, log_entries, sample_around, scratch_dir, sequencer_config};

const NOW: i64 = 1_792_504_800_000;
const VAULT: AccountId = AccountId([0xaa; 20]);

#[tokio::test]
async fn a_v24_build_refuses_unsigned_claimants_while_activation_waits_for_trusted_time() {
    let node = Node::start_with_time("startup-auth", true, false).await;
    let mut request = order(&key(1), 0);
    request.as_object_mut().unwrap().remove("auth");
    let (status, body) = node.post("/v1/orders", request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "missing signature");
    assert!(node.entries().is_empty(), "an unauthenticated startup caller never claims a logged account action");
    assert!(node.source.lock().unwrap().requests.is_empty());
}

#[tokio::test]
async fn a_pending_deposit_admits_the_signed_declaration_that_credits_it_and_headers_cannot_choose_geo() {
    let node = Node::start("declaration-peer", true).await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    let unsigned = json!({"account":account,"class":"professional"});
    let before = node.entries().len();
    assert_eq!(node.post("/v1/eligibility", unsigned).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, body) = node.post("/v1/eligibility", declaration(&client, 0)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no deposit is pending for the account yet: {body}");
    assert_eq!(node.entries().len(), before);
    assert!(node.source.lock().unwrap().requests.is_empty());
    let pending = node.post("/v1/dev/deposit", json!({"account":account,"amount":"100000"})).await.1;
    assert_eq!(pending[0]["type"], "deposit_pending");
    assert_eq!(pending[0]["reason"], "eligibility_required");
    assert_eq!(node.attest(&client).await[0]["type"], "eligibility_attested");
    let eligible = node.get(&format!("/v1/accounts/{account}/eligibility")).await;
    assert_eq!(eligible["class"], "professional");
    assert_eq!(eligible["statement"], "I attest that I am a professional client.");
    let reference = pending[0]["chain_ref"].as_str().unwrap().to_owned();
    assert_eq!(node.get(&format!("/v1/access/pending/{reference}")).await["amount"], "100000");
    node.retry_pending(&reference).await;
    let (status, events) = node.post("/v1/orders", order(&client, 1)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "fill"), "{events}");
    for _ in 0..3 {
        node.state.submit(Command::Tick).await.unwrap();
    }
    let proof: engine_core::merkle::AccountProof =
        serde_json::from_value(node.get(&format!("/v1/accounts/{account}/proof")).await).unwrap();
    assert!(proof.verify(proof.root));
    assert_eq!(proof.leaf.balance, Fixed::from_int(100000));
    assert_eq!(proof.positions.len(), 1, "the current proof includes the credited account's actual fill");
    let observed = node.source.lock().unwrap().requests.clone();
    assert_eq!(observed.last().unwrap()["peer"], "127.0.0.1", "spoofed forwarding headers never reach the source");
    let encoded = serde_json::to_string(&node.entries()).unwrap();
    assert!(!encoded.contains("127.0.0.1") && !encoded.contains("203.0.113.99"));
    assert_eq!(replay(&node.entries()).unwrap().engine.next_request_nonce(account), 2);
}

#[tokio::test]
async fn excluded_evm_orders_and_withdrawals_are_publicly_refused_before_the_unsupported_payout_guard() {
    let node = Node::start("evm-refusals", false).await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    for (jurisdiction, expected) in [
        (Jurisdiction::UnitedStates, "united_states"),
        (Jurisdiction::Sanctioned, "sanctioned_jurisdiction"),
        (Jurisdiction::Unknown, "jurisdiction_unknown"),
    ] {
        node.source.lock().unwrap().jurisdiction = jurisdiction;
        let nonce = node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap();
        let refused = node.post("/v1/orders", order(&client, nonce)).await.1;
        assert_eq!(refused[0]["reason"], json!({"kind":"access","reason":expected}));
        let withdrawal = withdrawal(&client, nonce + 1);
        let refused = node.post("/v1/withdrawals", withdrawal.clone()).await.1;
        assert_eq!(refused[0]["type"], "withdrawal_rejected");
        assert_eq!(refused[0]["reason"], json!({"kind":"access","reason":expected}));
        node.source.lock().unwrap().jurisdiction = Jurisdiction::Allowed;
        assert_eq!(node.post("/v1/withdrawals", withdrawal).await.0, StatusCode::CONFLICT);
    }
    let nonce = node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap();
    let refused = node.post("/v1/withdrawals", withdrawal(&client, nonce)).await.1;
    assert_eq!(refused[0]["reason"]["kind"], "payout_unavailable");
    let entries = node.entries();
    assert!(entries.iter().any(|entry| matches!(entry, LogEntry::Command(command)
        if matches!(command.sequenced.cmd, Command::AccessWithdraw { payout_available: false, .. }))));
    assert_eq!(replay(&entries).unwrap().engine.next_request_nonce(account), nonce + 1);
}

#[tokio::test]
async fn account_sanctions_and_bad_source_signatures_fail_closed_without_a_bogus_claimant_action() {
    let node = Node::start("account-screening", true).await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    node.source.lock().unwrap().sanctioned.insert(account);
    let refused = node.post("/v1/orders", order(&client, 1)).await.1;
    assert_eq!(refused[0]["reason"]["reason"], "account_sanctioned");
    node.source.lock().unwrap().sanctioned.clear();
    node.source.lock().unwrap().forged = true;
    let refused = node.post("/v1/orders", order(&client, 2)).await.1;
    assert_eq!(refused[0]["reason"]["reason"], "source_invalid");
    let mut forged = order(&key(2), 3);
    forged["account"] = json!(account);
    let (entries, queries) = (node.entries().len(), node.source.lock().unwrap().requests.len());
    assert_eq!(node.post("/v1/orders", forged).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(node.entries().len(), entries);
    assert_eq!(node.source.lock().unwrap().requests.len(), queries);
    assert_eq!(node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap(), 3);
}

#[tokio::test]
async fn an_allowed_mock_withdrawal_is_debited_signed_and_strictly_replayed_from_its_access_envelope() {
    let mut node = Node::start("mock-withdrawal", true).await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    let approved = node.post("/v1/withdrawals", withdrawal(&client, 1)).await.1;
    assert_eq!(approved[0]["type"], "withdrawal_queued");
    assert_eq!(approved[1]["type"], "withdrawal_approved");
    let signed = node.withdrawals.try_recv().unwrap();
    assert_eq!(
        (signed.account, signed.amount, signed.chain_id, signed.vault),
        (account, Fixed::from_int(10), 31_337, VAULT)
    );
    let replayed = replay_with_signer(
        &node.entries(),
        crate::log::CheckpointDomain { chain_id: 31_337, vault: VAULT },
        AccountId(EnclaveKey::test_key().address()),
    )
    .unwrap();
    assert_eq!(replayed.engine.accounts()[&account].balance, Fixed::from_int(99990));
    assert_eq!(replayed.engine.accounts()[&account].withdrawal_nonce, 1);
    assert_eq!(replayed.engine.next_request_nonce(account), 2);
}

#[tokio::test]
async fn concurrent_signed_orders_cannot_fill_twice_from_one_nonce() {
    let node = Node::start("access-nonce-race", true).await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    let request = order(&client, 1);
    let (first, second) = tokio::join!(node.post("/v1/orders", request.clone()), node.post("/v1/orders", request));
    let mut fills = 0;
    for (status, events) in [first, second] {
        if status == StatusCode::CONFLICT {
            assert_eq!(events, json!({"error":"invalid nonce","expected":2}));
        } else {
            assert_eq!(status, StatusCode::OK);
            for event in events.as_array().unwrap() {
                if event["type"] == "fill" {
                    fills += 1;
                }
                if event["type"] == "order_rejected" {
                    assert_eq!(event["reason"], json!({"kind":"invalid_nonce","expected":2}));
                }
            }
        }
    }
    assert_eq!(fills, 1);
    assert_eq!(
        node.state
            .query(move |engine| (engine.next_request_nonce(account), engine.accounts()[&account].positions[&1].qty))
            .await
            .unwrap(),
        (2, "0.01".parse().unwrap())
    );
    assert_eq!(replay(&node.entries()).unwrap().engine.next_request_nonce(account), 2);
}

#[tokio::test]
async fn a_client_can_cancel_its_resting_order_while_geo_access_is_excluded() {
    let node = Node::start("access-cancel", true).await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    let mut request: OrderRequest = serde_json::from_value(order(&client, 1)).unwrap();
    request.kind = OrderKind::Limit { price: Fixed::from_int(90000) };
    let hash = eip712::order_hash(&eip712::domain(31_337, VAULT), &request, 1, NOW + 60_000);
    request.auth.as_mut().unwrap().signature = eip712::sign(&client, &hash);
    let accepted = node.post("/v1/orders", json!(request)).await.1;
    assert_eq!(accepted[0]["type"], "order_accepted");
    let order_id = accepted[0]["order_id"].as_u64().unwrap();
    node.source.lock().unwrap().jurisdiction = Jurisdiction::UnitedStates;
    let queries = node.source.lock().unwrap().requests.len();
    let hash = eip712::cancel_hash(&eip712::domain(31_337, VAULT), account, order_id, 2, NOW + 60_000);
    let request = engine_core::command::CancelRequest {
        account,
        order_id,
        auth: Some(RequestAuth { nonce: 2, expires_at: NOW + 60_000, signature: eip712::sign(&client, &hash) }),
    };
    let (status, events) = node.post("/v1/orders/cancel", json!(request)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(events[0]["type"], "order_cancelled");
    assert_eq!(events[0]["reason"], "client");
    assert_eq!(node.source.lock().unwrap().requests.len(), queries);
    assert!(node.state.query(move |engine| engine.accounts()[&account].orders.is_empty()).await.unwrap());
}

struct Node {
    app: Router,
    state: ApiState,
    clock: Arc<TestClock>,
    dir: PathBuf,
    source: Arc<Mutex<source::Source>>,
    withdrawals: mpsc::UnboundedReceiver<SignedWithdrawal>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.tasks.iter().for_each(JoinHandle::abort);
    }
}

impl Node {
    async fn start(name: &str, mock_custody: bool) -> Self {
        Self::start_with_time(name, mock_custody, true).await
    }

    async fn start_with_time(name: &str, mock_custody: bool, trusted: bool) -> Self {
        let dir = scratch_dir(&format!("access-api-{name}"));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source = Arc::new(Mutex::new(source::Source {
            jurisdiction: Jurisdiction::Allowed,
            sanctioned: BTreeSet::new(),
            forged: false,
            requests: Vec::new(),
        }));
        let config: AccessSection = toml::from_str(&format!(
            "source='{}'\nendpoint='http://{}'\ntimeout='2s'\nmax_age='1s'\nmax_response_bytes=4096\nmax_in_flight=2",
            eip712::address(key(0x77).verifying_key()),
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let source_app = Router::new().route("/", post(source::answer)).with_state(source.clone());
        let source_task = tokio::spawn(async move {
            axum::serve(listener, source_app).await.unwrap();
        });
        let client = Arc::new(AccessClient::new(&config).unwrap());
        let enclave = Arc::new(EnclaveKey::test_key());
        let (stream, _) = broadcast::channel(1024);
        let (withdrawals, withdrawals_rx) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let stamps = Arc::new(TestClock::at(NOW));
        let operator = Operator { chain_id: 31_337, vault: VAULT, ..dev_config().operator().unwrap() };
        let cfg = crate::sequencer::SequencerConfig {
            access_policy: Some(config.policy()),
            chain_id: 31_337,
            vault: VAULT,
            operator,
            clock: stamps.clone(),
            ..sequencer_config(&dir, manifest_tx)
        };
        let mut sequencer = Sequencer::recover(cfg, enclave.clone(), stream.clone(), withdrawals).unwrap();
        sequencer.activate_protocol().unwrap();
        sequencer.genesis(&dev_config().genesis).unwrap();
        if trusted {
            sequencer.trust(sample_around(stamps.as_ref())).unwrap();
        }
        let (tx, rx) = mpsc::channel(64);
        let sequencer_task = tokio::spawn(async move {
            sequencer.run(rx).await.unwrap();
        });
        let state = ApiState {
            protocol: ProtocolVersion::LATEST,
            access: Some(client),
            tx,
            stream,
            attestation: Arc::new(
                attest_bundle(
                    &MockAttestor,
                    &AttestationBundle {
                        enclave_address: enclave.address(),
                        config_sha256: [0; 32],
                        tls_public_key_sha256: [0; 32],
                    },
                )
                .unwrap(),
            ),
            auth: ClientAuth::Unsigned,
            operator,
            mock_custody,
            payouts_enabled: mock_custody,
            clock: stamps.clone(),
            log_dir: dir.clone(),
            manifest,
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            // Buckets that refill once an hour, so no test depends on its speed.
            limits: Arc::new(ApiLimits::with_quota(
                &dev_config().api,
                |rate: RateLimit| Quota::with_period(Duration::from_secs(3600)).unwrap().allow_burst(rate.burst),
                DefaultClock::default(),
            )),
            history: Reader::new(dir.join(INDEX_FILE), watch::channel(true).1),
            marks: crate::test_support::marks_reader(),
            emission_budget: None,
            collateral: None,
            listing: Default::default(),
            signing: crate::test_support::signing(),
        };
        assert_eq!(
            state.query(Engine::protocol_version).await.unwrap(),
            if trusted { ProtocolVersion::LATEST } else { ProtocolVersion::V2 }
        );
        let quotes = ["a", "b"]
            .map(|source| SourceQuote { source: source.into(), mid: Fixed::from_int(100000), ts: NOW, contract: None })
            .to_vec();
        if trusted {
            state.submit(Command::Prices { market: 1, quotes }).await.unwrap();
        }
        let app = router(state.clone()).layer(Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)))));
        Self {
            app,
            state,
            clock: stamps,
            dir,
            source,
            withdrawals: withdrawals_rx,
            tasks: vec![source_task, sequencer_task],
        }
    }

    /// Re-trusts the advanced clock so stamps and queries stay admitted, without sequencing
    /// anything: the last sequenced time is left where it was.
    async fn retrust(&self) {
        self.state.tx.send(sequencer::Request::TimeSample(sample_around(&*self.clock))).await.unwrap();
    }

    /// Moves the sequencer's time forward by `ms`, re-trusting it so stamps stay admitted, and
    /// sequences a Tick, so the last sequenced time the API's checks read catches up.
    async fn move_time(&self, ms: u64) {
        self.clock.advance(Duration::from_millis(ms));
        self.retrust().await;
        self.state.submit(Command::Tick).await.unwrap();
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let request = Request::post(path)
            .header("content-type", "application/json")
            .header("x-forwarded-for", "203.0.113.99")
            .header("forwarded", "for=203.0.113.99")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({"error":String::from_utf8_lossy(&bytes)})))
    }

    async fn get(&self, path: &str) -> Value {
        let response = self.app.clone().oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap()
    }

    fn entries(&self) -> Vec<LogEntry> {
        log_entries(&self.dir)
    }

    /// Credits 100000 to `client`'s account the way a new client is onboarded: its own deposit, screened
    /// as the custody adapter screens it, waits pending until the client declares, and a retry with
    /// fresh evidence then credits it.
    async fn onboard(&self, client: &SigningKey) {
        let account = eip712::address(client.verifying_key());
        let reference = format!("0xfunding:{account}");
        let chain_ref = reference.clone();
        let deposit = self
            .state
            .query(move |engine| {
                crate::test_support::access::funding(
                    engine,
                    Some(account),
                    Some(account),
                    Fixed::from_int(100000),
                    &chain_ref,
                )
            })
            .await
            .unwrap();
        let events = self.state.submit(deposit).await.unwrap();
        assert!(matches!(events.as_slice(), [Event::DepositPending { .. }]), "{events:?}");
        assert_eq!(self.attest(client).await[0]["type"], "eligibility_attested");
        self.retry_pending(&reference).await;
    }

    /// Sequences a retry of the pending `reference` with fresh clear evidence; it must credit.
    async fn retry_pending(&self, reference: &str) {
        let chain_ref = reference.to_owned();
        let retry = self
            .state
            .query(move |engine| {
                let deposit = engine.pending_deposits()[&chain_ref].clone();
                let hash = eip712::deposit_screening_hash(&deposit);
                let evidence = crate::test_support::access::proof(engine, hash, deposit.account, deposit.payer);
                Command::RetryDepositScreening { chain_ref, evidence }
            })
            .await
            .unwrap();
        let events = self.state.submit(retry).await.unwrap();
        assert!(matches!(events.as_slice(), [Event::Deposited { .. } | Event::HouseCapitalMoved { .. }]), "{events:?}");
    }

    async fn attest(&self, client: &SigningKey) -> Value {
        let (status, events) = self.post("/v1/eligibility", declaration(client, 0)).await;
        assert_eq!(status, StatusCode::OK, "{events}");
        events
    }
}

/// A signed professional declaration for `client`'s account at `nonce`.
fn declaration(client: &SigningKey, nonce: u64) -> Value {
    let account = eip712::address(client.verifying_key());
    let class = EligibilityClass::Professional;
    let hash = eip712::eligibility_hash(&eip712::domain(31_337, VAULT), account, class, nonce, NOW + 60_000);
    let signature = eip712::sign(client, &hash);
    json!(EligibilityRequest { account, class, auth: RequestAuth { nonce, expires_at: NOW + 60_000, signature } })
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_slice(&[seed; 32]).unwrap()
}

fn order(key: &SigningKey, nonce: u64) -> Value {
    let account = eip712::address(key.verifying_key());
    let mut request = OrderRequest {
        account,
        client_id: 1,
        market: 1,
        side: Side::Buy,
        qty: "0.01".parse().unwrap(),
        kind: OrderKind::Market { quoted: Fixed::from_int(100000), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    };
    let hash = eip712::order_hash(&eip712::domain(31_337, VAULT), &request, nonce, NOW + 60_000);
    request.auth = Some(RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(key, &hash) });
    json!(request)
}

fn withdrawal(key: &SigningKey, nonce: u64) -> Value {
    let account = eip712::address(key.verifying_key());
    let amount = Fixed::from_int(10);
    let hash = eip712::withdrawal_hash(&eip712::domain(31_337, VAULT), account, amount, nonce, NOW + 60_000);
    json!(WithdrawalRequest {
        account,
        amount,
        auth: Some(RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(key, &hash) })
    })
}
