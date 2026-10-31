//! V19 API tests for the operator's admin route, and the operator channel's dev scenario through
//! `engine admin`.

use super::*;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::connect_info::MockConnectInfo;
use axum::http::Request as HttpRequest;
use clap::Parser;
use engine_core::auth::MAX_EXPIRY_AHEAD_MS;
use engine_core::command::{AdminCommand, AdminRequest, Command};
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::event::Event;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use governor::Quota;
use governor::clock::DefaultClock;
use k256::ecdsa::SigningKey;
use serde_json::Value;
use tokio::net::TcpListener;
use tower::ServiceExt;

use crate::admin::{self, AdminArgs, HexKey, KeyEnv, NodeApi, sign};
use crate::clock::test_clock::TestClock;
use crate::config::RateLimit;
use crate::history::Reader;
use crate::limits::ApiLimits;
use crate::log::LogEntry;
use crate::sequencer::{Sequencer, SequencerConfig, replay};
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{DEV_CONFIG, dev_config, log_entries, sample_around, scratch_dir, sequencer_config};
use crate::tls::{TlsIdentity, serve};

mod corporate;
mod election;
mod tls;

/// The dev operator key, whose public key `config/dev.toml` names.
const DEV_KEY: [u8; 32] = [0x42; 32];
/// The time the test sequencer stamps every command with, and the API and `engine admin` count
/// expiries from: Tue 20 Oct 2026 14:00 UTC.
const NOW: Millis = 1_792_504_800_000;
/// A trader on the dev node, which takes unsigned requests.
const TRADER: &str = "0x00000000000000000000000000000000000000aa";

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    args: AdminArgs,
}

#[tokio::test]
async fn a_signed_admin_command_is_applied_and_logged_with_the_operators_signature() {
    let node = Node::start("applied").await;
    let req = node.sign(0, reduce_only(Some(1)), NOW + 60_000);
    let (status, events) = node.post("/v1/admin", serde_json::to_value(&req).unwrap()).await;
    assert_eq!((status, events), (StatusCode::OK, serde_json::json!([{ "type": "admin_applied" }])));

    let entries = node.entries();
    let logged: Vec<_> = admin_entries(&entries);
    assert_eq!(logged, [(req.clone(), vec![Event::AdminApplied])]);
    let replayed = replay(&entries).unwrap().engine;
    assert!(replayed.markets()[&1].reduce_only);
    assert_eq!(replayed.next_request_nonce(node.state.operator.address), 1);
}

/// The node's clock once a rotation logged at `NOW` has run its seven days.
fn after_the_notice() -> Millis {
    NOW + engine_core::operator_bounds::NOTICE_MS + 1_000
}

#[tokio::test]
async fn a_rotated_key_reaches_the_log_only_once_its_notice_has_run_and_the_old_key_leaves_it() {
    let mut node = Node::start_on("rotation", true).await;
    let next = SigningKey::from_slice(&[0x55; 32]).unwrap();
    let address = engine_core::eip712::address(next.verifying_key());
    let rotation = node.sign(0, AdminCommand::RotateOperator { address }, NOW + 60_000);
    let (status, events) = node.post("/v1/admin", serde_json::to_value(&rotation).unwrap()).await;
    assert_eq!((status, events), (StatusCode::OK, serde_json::json!([{ "type": "admin_applied" }])));

    // Until then the new key stays out of the log, as a stranger's does, so a copy of its request
    // cannot be sent again once it is the operator.
    let early = sign(&next, &node.state.operator, reduce_only(Some(1)), 0, NOW + 60_000);
    let stranger =
        sign(&SigningKey::from_slice(&[7; 32]).unwrap(), &node.state.operator, reduce_only(Some(1)), 0, NOW + 60_000);
    let logged = node.entries().len();
    for req in [&early, &stranger] {
        let (status, body) = node.post("/v1/admin", serde_json::to_value(req).unwrap()).await;
        assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid signature")));
    }
    assert_eq!(node.entries().len(), logged);

    // After the notice the log's operator is the new key: it applies, and the old key is refused
    // before the log.
    node.stamps.advance(Duration::from_millis(after_the_notice() as u64 - NOW as u64));
    let sample = sample_around(node.stamps.as_ref());
    node.state.tx.send(crate::sequencer::Request::TimeSample(sample)).await.unwrap();
    node.state.clock = after_the_notice;
    let expires_at = after_the_notice() + 60_000;
    let handover = sign(&next, &node.state.operator, reduce_only(Some(1)), 0, expires_at);
    let (status, events) = node.post("/v1/admin", serde_json::to_value(&handover).unwrap()).await;
    assert_eq!(
        (status, events.as_array().and_then(|e| e.last()).cloned()),
        (StatusCode::OK, Some(serde_json::json!({ "type": "admin_applied" })))
    );
    let old = node.sign(1, reduce_only(Some(2)), expires_at);
    let logged = node.entries().len();
    let (status, body) = node.post("/v1/admin", serde_json::to_value(&old).unwrap()).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid signature")));
    assert_eq!(node.entries().len(), logged);
}

#[tokio::test]
async fn a_rotation_to_a_published_test_key_is_refused_before_the_log_under_real_attestation() {
    let mut node = Node::start_on("rotation-published", true).await;
    // The dev operator's key is one the repository publishes; rotating to it would hand a real enclave
    // an operator key anyone can read.
    let published = AdminCommand::RotateOperator { address: node.state.operator.address };
    let logged = node.entries().len();
    let mut real = (*node.state.attestation).clone();
    real.provider = "tdx".into();
    node.state.attestation = Arc::new(real);
    let (status, body) =
        node.post("/v1/admin", serde_json::to_value(node.sign(0, published.clone(), NOW + 60_000)).unwrap()).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("the operator may not rotate to a published test key"))
    );
    assert_eq!(node.entries().len(), logged);
    // The mock enclave leaves it to the engine, which refuses its own key.
    let mock = Node::start_on("rotation-published-mock", true).await;
    let (status, events) =
        mock.post("/v1/admin", serde_json::to_value(mock.sign(0, published, NOW + 60_000)).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(events[0]["type"], "admin_rejected", "{events}");
}

#[tokio::test]
async fn an_admin_request_the_operator_did_not_sign_stays_out_of_the_log() {
    let node = Node::start("unsigned").await;
    let expires_at = NOW + 60_000;
    let forged =
        sign(&SigningKey::from_slice(&[7; 32]).unwrap(), &node.state.operator, reduce_only(None), 0, expires_at);
    let mut altered = node.sign(0, reduce_only(Some(1)), expires_at);
    altered.command = reduce_only(None);
    let elsewhere = Operator { chain_id: 1, ..node.state.operator };
    let other_chain = sign(&SigningKey::from_slice(&DEV_KEY).unwrap(), &elsewhere, reduce_only(None), 0, expires_at);
    let logged = node.entries().len();
    for req in [forged, altered, other_chain] {
        let (status, body) = node.post("/v1/admin", serde_json::to_value(&req).unwrap()).await;
        assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid signature")));
    }
    assert_eq!(node.entries().len(), logged);
}

#[tokio::test]
async fn a_reused_ahead_or_expired_admin_request_stays_out_of_the_log() {
    let node = Node::start("stale").await;
    let first = node.sign(0, reduce_only(Some(1)), NOW + 60_000);
    assert_eq!(node.post("/v1/admin", serde_json::to_value(&first).unwrap()).await.0, StatusCode::OK);
    // The log writer flushes behind the reply and ticks keep sequencing, so count only signed
    // admin commands, waiting until the first is durable before counting what stays out.
    let signed = |node: &Node| {
        node.entries()
            .iter()
            .filter(|entry| {
                matches!(entry, LogEntry::Command(command) if matches!(&command.sequenced.cmd, Command::SignedAdmin(_)))
            })
            .count()
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while signed(&node) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the admin command is logged");
    let logged = signed(&node);

    let invalid_nonce = (StatusCode::CONFLICT, serde_json::json!({ "error": "invalid nonce", "expected": 1 }));
    assert_eq!(node.post("/v1/admin", serde_json::to_value(&first).unwrap()).await, invalid_nonce);
    let ahead = node.sign(5, reduce_only(Some(2)), NOW + 60_000);
    assert_eq!(node.post("/v1/admin", serde_json::to_value(&ahead).unwrap()).await, invalid_nonce);
    let expired = node.sign(1, reduce_only(Some(2)), NOW - 1);
    let (status, body) = node.post("/v1/admin", serde_json::to_value(&expired).unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().starts_with("request expired at"), "{body}");
    assert_eq!(signed(&node), logged);

    // An expiry too far ahead is the engine's to refuse, so the nonce cannot be used later.
    let too_far = node.sign(1, reduce_only(Some(2)), NOW + MAX_EXPIRY_AHEAD_MS + 60_000);
    let (status, events) = node.post("/v1/admin", serde_json::to_value(&too_far).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(events[0]["type"], "admin_rejected", "{events}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while signed(&node) < logged + 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the refusal is logged");
    assert_eq!(signed(&node), logged + 1);
}

#[tokio::test]
async fn only_an_admin_request_the_node_would_sequence_spends_the_operators_rate() {
    let mut node = Node::start("admin-rate").await;
    // Buckets that refill hourly, so only the burst counts however long the requests take.
    let hourly = |rate: RateLimit| Quota::with_period(Duration::from_secs(3600)).unwrap().allow_burst(rate.burst);
    node.state.limits = Arc::new(ApiLimits::with_quota(&dev_config().api, hourly, DefaultClock::default()));
    let burst = u64::from(dev_config().api.per_account.burst.get());
    let request = |nonce| serde_json::to_value(node.sign(nonce, reduce_only(Some(1)), NOW + 60_000)).unwrap();
    assert_eq!(node.post("/v1/admin", request(0)).await.0, StatusCode::OK);
    // A replay fails its nonce check first and spends nothing, or the burst below would run out early.
    for _ in 0..6 {
        assert_eq!(node.post("/v1/admin", request(0)).await.0, StatusCode::CONFLICT);
    }
    for nonce in 1..burst {
        assert_eq!(node.post("/v1/admin", request(nonce)).await.0, StatusCode::OK, "{nonce}");
    }
    assert_eq!(node.post("/v1/admin", request(burst)).await.0, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn a_command_only_the_node_sequences_is_refused_before_the_log() {
    let node = Node::start("node-only").await;
    let node_only = [
        AdminCommand::SetClientAuth(ClientAuth::Unsigned),
        AdminCommand::SetOperator(node.state.operator),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST },
        AdminCommand::ActivateCustodyFinality { version: ProtocolVersion::LATEST, legacy_deposits: vec![] },
    ];
    let logged = node.entries().len();
    for command in node_only {
        let req = node.sign(0, command, NOW + 60_000);
        let (status, body) = node.post("/v1/admin", serde_json::to_value(&req).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    assert_eq!(node.entries().len(), logged);
}

#[tokio::test]
async fn the_market_view_shows_every_market_reduce_only() {
    let node = Node::start("market-view").await;
    let req = node.sign(0, reduce_only(None), NOW + 60_000);
    assert_eq!(node.post("/v1/admin", serde_json::to_value(&req).unwrap()).await.0, StatusCode::OK);
    let (_, markets) = node.get("/v1/markets").await;
    let flags: Vec<&Value> = markets.as_array().unwrap().iter().map(|m| &m["reduce_only"]).collect();
    assert!(!flags.is_empty() && flags.iter().all(|flag| **flag == true), "{markets}");
}

#[tokio::test]
async fn engine_admin_fails_when_the_engine_rejects_the_command() {
    let node = Node::start("cli-rejected").await;
    let url = format!("https://{}", node.serve().await);
    let key = || KeyEnv { key_file: None, key: Some(HexKey(format!("0x{}", hex::encode(DEV_KEY)))) };
    // The dev parameters have three stress levels, so level 9 does not exist.
    let err = admin::run_with_clock(node.args(&url, &["set-stress-floor", "9"]), key(), || NOW).await.unwrap_err();
    assert!(err.to_string().contains("rejected the command: stress level 9 does not exist"), "{err:#}");
    let logged = admin_entries(&node.entries());
    assert!(matches!(&logged[..], [(_, events)] if matches!(events[..], [Event::AdminRejected { .. }])));
}

/// The feature's dev scenario: the operator lists a new market with `engine admin`, the same signed
/// request sent again is refused, and with every market reduce-only an open is refused while a
/// close fills.
#[tokio::test]
async fn dev_scenario_list_a_market_once_then_every_market_reduce_only_refuses_opens_and_fills_closes() {
    let node = Node::start("dev-scenario").await;
    let url = format!("https://{}", node.serve().await);
    let dir = scratch_dir("dev-scenario-files");
    fs::create_dir_all(&dir).unwrap();
    let sol = dir.join("sol.toml");
    let spec =
        toml::to_string(&MarketSpec { id: 3, symbol: "SOL-USD".into(), ..dev_config().genesis.markets[0].clone() });
    fs::write(&sol, spec.unwrap()).unwrap();
    let key = || KeyEnv { key_file: None, key: Some(HexKey(format!("0x{}", hex::encode(DEV_KEY)))) };

    let listing =
        admin::run_with_clock(node.args(&url, &["upsert-market", sol.to_str().unwrap()]), key(), || NOW).await.unwrap();
    let err = admin::submit(&node.api(&url), &listing).await.unwrap_err();
    assert!(err.to_string().contains("409 Conflict"), "{err:#}");
    let (_, markets) = node.get("/v1/markets").await;
    let listed = markets.as_array().unwrap().iter().filter(|m| m["symbol"] == "SOL-USD").count();
    assert_eq!(listed, 1, "{markets}");

    node.fund_and_buy_btc().await;
    admin::run_with_clock(node.args(&url, &["set-reduce-only", "true"]), key(), || NOW).await.unwrap();
    let (_, opened) = node.post("/v1/orders", btc_order("buy")).await;
    assert_eq!(opened[0]["reason"]["kind"], "reduce_only", "{opened}");
    let (_, closed) = node.post("/v1/orders", btc_order("sell")).await;
    let fill = closed.as_array().unwrap().iter().find(|e| e["type"] == "fill").expect("the close fills");
    assert_eq!(fill["position"], "0", "{closed}");

    // The log holds both admin commands with the operator's signature, and replays.
    let entries = node.entries();
    let logged = admin_entries(&entries);
    assert_eq!(logged.len(), 2);
    assert_eq!(logged[0], (listing, vec![Event::MarketOpened { market: 3 }, Event::AdminApplied]));
    let replayed = replay(&entries).unwrap().engine;
    assert!(replayed.markets().values().all(|market| replayed.is_reduce_only(market)));
}

#[tokio::test]
async fn an_emission_schedule_must_have_the_period_budget_the_vaults_cap_was_derived_from() {
    let mut node = Node::start("emission-budget").await;
    node.state.emission_budget = Some(TokenUnits(1_000));
    let init =
        |budget| AdminCommand::InitEmissions(EmissionSchedule { start_ms: NOW, period_budget: TokenUnits(budget) });
    let (status, body) =
        node.post("/v1/admin", serde_json::to_value(node.sign(0, init(1_001), NOW + 60_000)).unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("1001 differs from the vault's 1000"), "{body}");
    assert!(admin_entries(&node.entries()).is_empty(), "a refused schedule stays out of the log");

    let (status, events) =
        node.post("/v1/admin", serde_json::to_value(node.sign(0, init(1_000), NOW + 60_000)).unwrap()).await;
    assert_eq!((status, events), (StatusCode::OK, serde_json::json!([{ "type": "admin_applied" }])));
}

// ── Helpers ──

/// A node API over a running sequencer on a fresh log, under the latest protocol with the dev
/// genesis and operator and unsigned client requests, as a dev node runs.
struct Node {
    state: ApiState,
    dir: PathBuf,
    tls: TlsIdentity,
    stamps: Arc<TestClock>,
    key: Arc<EnclaveKey>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Node {
    /// Recovers a sequencer on a fresh log named after `name`, activates the protocol, and sequences
    /// the dev genesis.
    async fn start(name: &str) -> Self {
        Self::start_on(name, false).await
    }

    /// `start`, under the latest protocol version instead of the legacy one when `latest`.
    async fn start_on(name: &str, latest: bool) -> Self {
        Self::start_with(name, latest, &dev_config().genesis).await
    }

    /// `start_on`, sequencing `genesis` instead of the dev genesis.
    async fn start_with(name: &str, latest: bool, genesis: &crate::config::Genesis) -> Self {
        let dir = scratch_dir(&format!("admin-{name}"));
        let key = Arc::new(EnclaveKey::generate());
        let (stream, _) = broadcast::channel(1024);
        let (withdrawals_tx, _) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let stamps = Arc::new(TestClock::at(NOW));
        let cfg = SequencerConfig { clock: stamps.clone(), ..sequencer_config(&dir, manifest_tx) };
        let operator = cfg.operator;
        let mut seq = Sequencer::recover(cfg, key.clone(), stream.clone(), withdrawals_tx).unwrap();
        if latest {
            seq.activate_protocol().unwrap();
        } else {
            seq.activate_legacy_protocol().unwrap();
        }
        seq.genesis(genesis).unwrap();
        // Trusted time agrees with the fixed clock, so the queued activation and genesis go in now.
        seq.trust(sample_around(stamps.as_ref())).unwrap();
        let (tx, rx) = mpsc::channel(1024);
        let task = tokio::spawn(seq.run(rx));
        let tls = TlsIdentity::generate().unwrap();
        let bundle = AttestationBundle {
            enclave_address: key.address(),
            config_sha256: [0; 32],
            tls_public_key_sha256: tls.public_key_sha256,
        };
        let state = ApiState {
            protocol: if latest { ProtocolVersion::LATEST } else { ProtocolVersion::V23 },
            access: None,
            tx,
            stream,
            attestation: Arc::new(attest_bundle(&MockAttestor, &bundle).unwrap()),
            auth: ClientAuth::Unsigned,
            operator,
            mock_custody: true,
            payouts_enabled: true,
            clock: || NOW,
            log_dir: dir.clone(),
            manifest,
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            limits: Arc::new(ApiLimits::new(&dev_config().api)),
            history: Reader::new(PathBuf::new(), watch::channel(true).1),
            marks: crate::test_support::marks_reader(),
            emission_budget: None,
            collateral: None,
            listing: Default::default(),
            signing: crate::test_support::signing(),
        };
        Node { state, dir, tls, stamps, key, task }
    }

    /// Serves the API on a free port of the IPv4 loopback.
    async fn serve(&self) -> SocketAddr {
        self.serve_on("127.0.0.1:0").await
    }

    /// Serves the API over TLS, as a node does, on `addr`, a free port of a loopback address.
    async fn serve_on(&self, addr: &str) -> SocketAddr {
        let listener = TcpListener::bind(addr).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tls, app, limits) = (self.tls.acceptors.clone(), router(self.state.clone()), self.state.limits.clone());
        tokio::spawn(serve(listener, tls, app, limits, dev_config().api));
        addr
    }

    /// `engine admin` against this node at `url`, pinned to its attested key, with the dev config,
    /// then `rest`.
    fn args(&self, url: &str, rest: &[&str]) -> AdminArgs {
        let pin = hex::encode(self.tls.public_key_sha256);
        let argv = ["engine", "--config", DEV_CONFIG, "--node", url, "--pin", &pin].into_iter();
        Cli::try_parse_from(argv.chain(rest.iter().copied())).unwrap().args
    }

    /// This node's API at `url`, pinned to its attested key.
    fn api(&self, url: &str) -> NodeApi {
        NodeApi { url: url.parse().unwrap(), pin: self.tls.public_key_sha256 }
    }

    /// `command` signed by the dev operator key with `nonce`, to expire at `expires_at`.
    fn sign(&self, nonce: u64, command: AdminCommand, expires_at: Millis) -> AdminRequest {
        sign(&SigningKey::from_slice(&DEV_KEY).unwrap(), &self.state.operator, command, nonce, expires_at)
    }

    /// Credits `TRADER`, prices BTC at 100000, and buys 0.1 BTC for it.
    async fn fund_and_buy_btc(&self) {
        let deposit = serde_json::json!({ "account": TRADER, "amount": "100000" });
        assert_eq!(self.post("/v1/dev/deposit", deposit).await.0, StatusCode::OK);
        let quotes = ["a", "b"]
            .map(|venue| SourceQuote { source: venue.into(), mid: "100000".parse().unwrap(), ts: NOW, contract: None })
            .to_vec();
        self.state.submit(Command::Prices { market: 1, quotes }).await.expect("the sequencer is running");
        let (_, bought) = self.post("/v1/orders", btc_order("buy")).await;
        assert!(bought.as_array().unwrap().iter().any(|e| e["type"] == "fill"), "{bought}");
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
        let app = router(self.state.clone()).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
}

/// `SetReduceOnly` on for `market`, or for every market.
fn reduce_only(market: Option<MarketId>) -> AdminCommand {
    AdminCommand::SetReduceOnly { market, reduce_only: true }
}

/// An unsigned 0.1 BTC market order for `TRADER` quoted at 100000, as a JSON body.
fn btc_order(side: &str) -> Value {
    serde_json::json!({
        "account": TRADER, "client_id": 1, "market": 1, "side": side, "qty": "0.1",
        "kind": { "kind": "market", "quoted": "100000" },
    })
}

/// Each signed admin command among `entries`, with the events logged with it.
fn admin_entries(entries: &[LogEntry]) -> Vec<(AdminRequest, Vec<Event>)> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(c) => match &c.sequenced.cmd {
                Command::SignedAdmin(req) => Some((AdminRequest::clone(req), c.events.clone().unwrap_or_default())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}
