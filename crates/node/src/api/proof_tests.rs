//! The account proof and emission entitlement routes under the historical V19 rules, and the state
//! tree's dev scenario: accounts credited through the API, checkpoint work that stays flat as they
//! grow, a proof that verifies against the logged root, and logs written before the state tree that
//! still replay.

use super::*;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;

use axum::body::{Body, to_bytes};
use axum::extract::connect_info::MockConnectInfo;
use axum::http::Request as HttpRequest;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712::B256;
use engine_core::emissions::{EmissionSchedule, PERIOD_MS, TokenUnits};
use engine_core::merkle::RootWork;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};
use governor::Quota;
use governor::clock::DefaultClock;
use serde_json::Value;
use tower::ServiceExt;

use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::config::RateLimit;
use crate::history::INDEX_FILE;
use crate::log::LogEntry;
use crate::sequencer::fixture_tests::replay_trusted as replay;
use crate::sequencer::{Sequencer, SequencerConfig};
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{dev_config, log_entries, sample_around, scratch_dir, sequencer_config};

/// Commands between checkpoints.
const CHECKPOINT_EVERY: u64 = 100;
/// The time the sequencer stamps every command with, the API checks against, and quotes carry:
/// Tue 20 Oct 2026 14:00 UTC.
const NOW: Millis = 1_792_504_800_000;
/// The account counts the dev scenario compares checkpoint work at; it credits up to the last.
const ACCOUNTS: [u32; 3] = [1_000, 10_000, 100_000];
/// Logs written before the state tree: by the node before protocol versions, and under `V3` and
/// `V4`.
const OLD_LOGS: [&str; 3] = [
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/v1-dev.jsonl"),
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v3.jsonl"),
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v4.jsonl"),
];

#[tokio::test]
async fn account_view_and_proof_expose_pending_principal_then_finality_releases_only_the_lock() {
    let node = Node::start("pending-principal").await;
    assert!(node.state.query(|e| e.deposit_finality_enabled()).await.unwrap());
    let trader = account(7);
    node.deposit(trader, "50").await;
    let receipt = DepositReceipt {
        block: 1,
        block_hash: B256::repeat_byte(1),
        tx_hash: B256::repeat_byte(2),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 0,
        beneficiary: DepositBeneficiary::Trader { account: trader },
        amount: "100".parse().unwrap(),
    };
    node.state
        .submit(Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: receipt.clone() })))
        .await
        .unwrap();
    let (status, view) = node.get(&format!("/v1/accounts/{trader}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["balance"], "150");
    assert_eq!(view["unfinalised_deposits"], "100");
    node.checkpoint().await;
    let (_, body) = node.get(&format!("/v1/accounts/{trader}/proof")).await;
    let proof: AccountProof = serde_json::from_value(body).unwrap();
    assert_eq!(proof.leaf.unfinalised_deposits, "100".parse().unwrap());
    assert!(proof.verify(logged_root(&node.entries(), proof.seq)));
    let reference = receipt.reference();
    let observed_at_seq =
        node.state.query(move |e| e.recorded_deposit(&reference).unwrap().updated_at_seq).await.unwrap();
    node.state
        .submit(Command::CustodyDeposit(Box::new(DepositUpdate::Finalise { receipt, observed_at_seq })))
        .await
        .unwrap();
    let (_, view) = node.get(&format!("/v1/accounts/{trader}")).await;
    assert_eq!(view["balance"], "150");
    assert!(view.get("unfinalised_deposits").is_none());
}

/// The feature's dev scenario: 100k accounts credited through the dev deposit route, with the work
/// of a checkpoint over the same number of deposits nearly the same at 1k, 10k, and 100k accounts;
/// a proof of one account's balance and position, from the API, that verifies against the root its
/// checkpoint logged and that a replay of the log reproduces; and logs written before the state tree
/// that still replay to their roots.
#[tokio::test]
async fn dev_scenario_checkpoint_work_follows_changes_a_proof_verifies_and_old_logs_replay() {
    let node = Node::start("proof-dev-scenario").await;
    let mut work: Vec<RootWork> = Vec::new();
    for n in 1..=ACCOUNTS[2] {
        node.deposit(account(n), "100").await;
        if ACCOUNTS.contains(&n) {
            work.push(
                node.state
                    .query(|e| e.last_root_work())
                    .await
                    .expect("the sequencer is running")
                    .expect("a root under V9"),
            );
        }
    }
    // Each checkpoint hashed 100 accounts, their 100 deposit references, and the remaining state.
    assert!(work.iter().all(|w| w.leaves == 201), "{work:?}");
    // Nodes grow with the tree's depth, the log of its leaves: a hundred times the accounts take
    // under three times the node hashes, where hashing the whole state would take a hundred times.
    assert!(work[2].nodes < 3 * work[0].nodes, "{work:?}");

    let trader = account(7);
    node.price_btc("100000").await;
    let order = OrderRequest {
        account: trader,
        client_id: 1,
        market: 1,
        side: Side::Buy,
        qty: "0.0005".parse().unwrap(),
        kind: OrderKind::Market { quoted: "100000".parse().unwrap(), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    };
    assert_eq!(node.post("/v1/orders", serde_json::to_value(order).unwrap()).await.0, StatusCode::OK);
    node.checkpoint().await;

    let (status, body) = node.get(&format!("/v1/accounts/{trader}/proof")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let proof: AccountProof = serde_json::from_value(body).unwrap();
    let entries = node.entries();
    assert!(proof.verify(logged_root(&entries, proof.seq)));
    let (_, view) = node.get(&format!("/v1/accounts/{trader}")).await;
    assert_eq!(serde_json::to_value(proof.leaf.balance).unwrap(), view["balance"]);
    let positions: BTreeMap<_, _> = proof.positions.iter().map(|held| (held.market, held.position)).collect();
    assert_eq!(serde_json::to_value(positions).unwrap(), view["positions"]);
    assert_eq!(proof.markets.len(), 1);
    assert_eq!(proof.markets[0].leaf.mark, Some("100000".parse().unwrap()));
    let replayed = replay(&entries).unwrap();
    assert_eq!(replayed.engine.account_proof(trader), Some(proof));
    // After the client auth and the operator are recorded.
    assert_eq!(replayed.activations, [(ProtocolVersion::V23, 2)]);

    for path in OLD_LOGS {
        let mut replayed = replay(&crate::sequencer::fixture_tests::signed_fixture(path.as_ref()))
            .unwrap_or_else(|e| panic!("{path}: {e:#}"));
        assert!(replayed.checkpoints > 0 && replayed.engine.protocol_version() < ProtocolVersion::V9, "{path}");
        // Its accounts have no proofs: their roots hash the state encoding.
        let held = *replayed.engine.accounts().keys().next().expect("the log credits an account");
        assert!(replayed.engine.account_proof(held).is_none(), "{path}");
        replayed.engine.state_root();
        assert!(replayed.engine.last_root_work().is_none(), "{path}");
    }
}

#[tokio::test]
async fn an_account_is_provable_from_the_first_checkpoint_that_holds_it() {
    let node = Node::start("proof-route").await;
    let (status, body) = node.get("/v1/accounts/0x12/proof").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    node.deposit(account(1), "100").await;
    // Credited, but no checkpoint holds it yet.
    assert_eq!(node.get(&format!("/v1/accounts/{}/proof", account(1))).await.0, StatusCode::NOT_FOUND);
    node.checkpoint().await;
    let (status, body) = node.get(&format!("/v1/accounts/{}/proof", account(1))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let proof: AccountProof = serde_json::from_value(body).unwrap();
    assert!(proof.verify(logged_root(&node.entries(), proof.seq)));
    assert!(proof.markets.is_empty());
    // An account never credited has no leaf.
    assert_eq!(node.get(&format!("/v1/accounts/{}/proof", account(2))).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_entitlement_is_provable_from_the_first_checkpoint_after_its_period() {
    let node = Node::start("emissions-route").await;
    let schedule = EmissionSchedule { start_ms: NOW, period_budget: TokenUnits(1_000) };
    let (status, events) = node.post("/v1/admin", node.signed(AdminCommand::InitEmissions(schedule))).await;
    assert_eq!((status, events.as_array().unwrap().last()), (StatusCode::OK, Some(&json!({"type": "admin_applied"}))));
    let trader = account(1);
    node.deposit(trader, "100000").await;
    for (side, mid) in [(Side::Buy, "100000"), (Side::Sell, "99000")] {
        node.price_btc(mid).await;
        let order = OrderRequest {
            account: trader,
            client_id: 1,
            market: 1,
            side,
            qty: "0.01".parse().unwrap(),
            kind: OrderKind::Market { quoted: mid.parse().unwrap(), max_slippage_bps: None },
            reduce_only: false,
            auth: None,
        };
        assert_eq!(node.post("/v1/orders", serde_json::to_value(order).unwrap()).await.0, StatusCode::OK);
    }
    node.checkpoint().await;
    // The loss earns nothing until its period is final and a checkpoint holds the allocation.
    assert_eq!(node.get(&format!("/v1/accounts/{trader}/emissions")).await.0, StatusCode::NOT_FOUND);
    node.advance(Duration::from_millis(PERIOD_MS as u64)).await;
    // The first command after the period's end finalises it.
    node.state.submit(Command::Tick).await.expect("the sequencer is running");
    node.checkpoint().await;
    let (status, body) = node.get(&format!("/v1/accounts/{trader}/emissions")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let proof: EntitlementProof = serde_json::from_value(body).unwrap();
    assert_eq!(proof.entitled, TokenUnits(1_000));
    assert!(proof.verify(logged_root(&node.entries(), proof.seq)));
    assert_eq!(node.get("/v1/accounts/0x12/emissions").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(node.get(&format!("/v1/accounts/{}/emissions", account(2))).await.0, StatusCode::NOT_FOUND);
}

// ── Helpers ──

/// A node on a fresh log under the latest protocol version with the dev genesis, serving the API
/// in process, stamping commands on a clock that moves only when the test moves it.
struct Node {
    app: Router,
    state: ApiState,
    dir: PathBuf,
    stamps: Arc<TestClock>,
}

impl Node {
    /// Recovers a sequencer on a fresh log named after `name`, activates the protocol, sequences
    /// the dev genesis, and serves the API over it, checkpointing every `CHECKPOINT_EVERY` commands.
    async fn start(name: &str) -> Self {
        let dir = scratch_dir(name);
        let key = Arc::new(EnclaveKey::test_key());
        let (stream, _) = broadcast::channel(16);
        let (withdrawals_tx, _) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let stamps = Arc::new(TestClock::at(NOW));
        let cfg = SequencerConfig {
            segment_commands: 10_000,
            checkpoint_every: CHECKPOINT_EVERY,
            checkpoint_parent: Default::default(),
            checkpoints: None,
            clock: stamps.clone(),
            ..sequencer_config(&dir, manifest_tx)
        };
        let mut seq = Sequencer::recover(cfg, key.clone(), stream.clone(), withdrawals_tx).unwrap();
        seq.activate_legacy_protocol().unwrap();
        seq.genesis(&dev_config().genesis).unwrap();
        // Trusted time agrees with the fixed clock, so the queued activation and genesis go in now.
        seq.trust(sample_around(stamps.as_ref())).unwrap();
        let (tx, rx) = mpsc::channel(1024);
        tokio::spawn(seq.run(rx));
        // The scenario credits 100k accounts from one client, so no request is rate limited.
        let unlimited = |_: RateLimit| Quota::per_second(NonZeroU32::MAX);
        let bundle = AttestationBundle {
            enclave_address: key.address(),
            config_sha256: [0; 32],
            tls_public_key_sha256: [0; 32],
        };
        let state = ApiState {
            protocol: engine_core::protocol::ProtocolVersion::V23,
            access: None,
            tx,
            stream,
            attestation: Arc::new(attest_bundle(&MockAttestor, &bundle).unwrap()),
            auth: ClientAuth::Unsigned,
            operator: dev_config().operator().unwrap(),
            mock_custody: true,
            payouts_enabled: true,
            clock: || NOW,
            log_dir: dir.clone(),
            manifest,
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            limits: Arc::new(ApiLimits::with_quota(&dev_config().api, unlimited, DefaultClock::default())),
            history: Reader::new(dir.join(INDEX_FILE), watch::channel(true).1),
            emission_budget: None,
            listing: Default::default(),
            signing: crate::test_support::signing(),
        };
        let app = router(state.clone()).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        Node { app, state, dir, stamps }
    }

    /// Moves the clock and trusted time forward by `by`.
    async fn advance(&self, by: Duration) {
        self.stamps.advance(by);
        let sample = sample_around(self.stamps.as_ref());
        self.state.tx.send(sequencer::Request::TimeSample(sample)).await.expect("the sequencer is running");
    }

    /// `command` signed by the dev operator key with the operator's next nonce, as `engine admin`
    /// posts it.
    fn signed(&self, command: AdminCommand) -> Value {
        let key = k256::ecdsa::SigningKey::from_slice(&[0x42; 32]).unwrap();
        let operator = dev_config().operator().unwrap();
        serde_json::to_value(crate::admin::sign(&key, &operator, command, 0, NOW + 60_000)).unwrap()
    }

    /// Credits `account` with `amount` through the dev deposit route.
    async fn deposit(&self, account: AccountId, amount: &str) {
        let body = serde_json::json!({ "account": account, "amount": amount });
        let (status, events) = self.post("/v1/dev/deposit", body).await;
        assert_eq!(status, StatusCode::OK, "{events}");
    }

    /// Prices BTC at `mid` from two venues, stamped now.
    async fn price_btc(&self, mid: &str) {
        let ts = self.stamps.now_ms();
        let quotes = ["a", "b"]
            .map(|venue| SourceQuote { source: venue.into(), mid: mid.parse().unwrap(), ts, contract: None })
            .to_vec();
        self.state.submit(Command::Prices { market: 1, quotes }).await.expect("the sequencer is running");
    }

    /// Sequences ticks until the last command applied was followed by a checkpoint.
    async fn checkpoint(&self) {
        while self.state.query(|e| e.last_seq().map_or(0, |s| s + 1)).await.expect("running") % CHECKPOINT_EVERY != 0 {
            self.state.submit(Command::Tick).await.expect("the sequencer is running");
        }
    }

    /// Every entry of the node's log, as flushed so far.
    fn entries(&self) -> Vec<LogEntry> {
        log_entries(&self.dir)
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

/// The account numbered `n`.
fn account(n: u32) -> AccountId {
    let mut id = [0x11; 20];
    id[16..].copy_from_slice(&n.to_be_bytes());
    AccountId(id)
}

/// The root the log's checkpoint at `seq` carries.
fn logged_root(entries: &[LogEntry], seq: u64) -> B256 {
    let root = entries
        .iter()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(cp) if cp.seq == seq => Some(&cp.root),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no checkpoint at seq {seq}"));
    root.parse().unwrap()
}
