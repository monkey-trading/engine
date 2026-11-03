//! A start's secrets: what it opens, what it holds for, and what it leaves off.

use super::*;

use std::path::PathBuf;

use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use crate::alerts;
use crate::handover::{receive, send};
use crate::metrics::Metrics;
use crate::secrets::delivery::{self, Hello, Offer, Reply};
use crate::secrets::tests::{CONFIG, RELEASE, bundle, config, domain_of, full, operator_key};
use crate::tee::sealing::{MockSealer, Sealer};
use crate::tee::secrets::file_for;
use crate::trusted_time::tests::logs::Logs;

mod wiring;

/// How long a test waits for what must happen; a hold is checked over a shorter wait.
const WAIT: Duration = Duration::from_secs(30);
const HOLDING: Duration = Duration::from_millis(300);
/// Shorter than a failed start's hold in a test build.
const UNDER_THE_HOLD: Duration = Duration::from_secs(4);

fn start() -> KeyStart {
    KeyStart { mock_launch_digest: Some(RELEASE), ..KeyStart::default() }
}

fn telemetry() -> Telemetry {
    let metrics = Arc::new(Metrics::new());
    let (_, alerts) = alerts::channel(alerts::Routes::default(), metrics.clone());
    Telemetry { metrics, alerts, pages_critical: false }
}

fn sealing_dir(cfg: &NodeConfig) -> PathBuf {
    cfg.tee.sealing.as_ref().unwrap().dir.clone()
}

fn listen(cfg: &NodeConfig) -> String {
    cfg.tee.sealing.as_ref().unwrap().secrets.as_ref().unwrap().listen.to_string()
}

/// Seals `bundle` for the release `cfg` runs, as an earlier start did.
fn sealed_earlier(cfg: &NodeConfig, bundle: crate::secrets::Bundle) {
    let sealer: Box<dyn Sealer> = Box::new(MockSealer { launch_digest: RELEASE });
    let store = SecretStore::open(&sealing_dir(cfg), sealer, CONFIG).unwrap();
    store.replace(bundle.sign(domain_of(cfg), &operator_key())).unwrap();
}

/// Runs `startup` on its own task, returning what it set.
type Started = (NodeConfig, Telemetry, anyhow::Result<Delivered>);
fn spawn_startup(mut cfg: NodeConfig) -> JoinHandle<Started> {
    tokio::spawn(async move {
        let mut telemetry = telemetry();
        let delivered = startup(&mut cfg, &mut telemetry, &start()).await;
        (cfg, telemetry, delivered)
    })
}

/// Delivers `bundle` to the listener at `address`, retrying until it is bound.
async fn deliver(cfg: &NodeConfig, address: &str, bundle: crate::secrets::Bundle) -> Reply {
    deliver_signed(cfg, address, bundle, &operator_key()).await
}

/// `deliver`, with the bundle signed by `key`: whose key the listener takes is itself the behaviour
/// under test.
async fn deliver_signed(
    cfg: &NodeConfig,
    address: &str,
    bundle: crate::secrets::Bundle,
    key: &k256::ecdsa::SigningKey,
) -> Reply {
    let mut stream = timeout(WAIT, async {
        loop {
            match TcpStream::connect(address).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("the listener never bound");
    let nonce = [0x5c; 32];
    send(&mut stream, &Hello { nonce: hex::encode(nonce) }).await.unwrap();
    let offer: Offer = receive(&mut stream).await.unwrap();
    let domain = domain_of(cfg);
    let public_key = hex::decode(&offer.public_key).unwrap();
    let release = crate::tee::sealing::Release { launch_digest: RELEASE, config_sha256: CONFIG };
    let signed = bundle.sign(domain, key);
    let message = delivery::encrypt(domain, &release, &public_key, &nonce, &signed).unwrap();
    send(&mut stream, &message).await.unwrap();
    receive(&mut stream).await.unwrap()
}

#[tokio::test]
async fn a_restart_opens_what_was_sealed_and_sets_it_everywhere_without_a_delivery() {
    let (logs, _guard) = Logs::capture();
    let cfg = config("startup-restart");
    sealed_earlier(&cfg, full(4, "a"));
    let (cfg, telemetry, delivered) = timeout(WAIT, spawn_startup(cfg)).await.expect("it held").unwrap();
    let delivered = delivered.unwrap();

    assert_eq!(delivered.store.as_ref().unwrap().version(), Some(4));
    let rpc = cfg.checkpoints.as_ref().unwrap().rpc_url.expose().unwrap();
    assert_eq!(rpc.as_str(), "https://rpc.example/v2/FAKE-chain-a");
    assert!(telemetry.pages_critical, "the delivered webhook routes Critical alerts");
    assert!(telemetry.alerts.pages_critical());
    let connection = &cfg.feeds.venues.as_ref().unwrap().connections[0];
    assert!(!delivered.withholds(connection));
    assert_eq!(delivered.api_key(connection).unwrap().unwrap().expose(), "FAKE-vendor-a");
    assert!(delivered.listener.is_some(), "the node goes on taking deliveries");
    assert_eq!(logs.critical("secret_missing"), 0);
    assert_eq!(logs.count("alert_resolved=\"secrets_restart_required\""), 1, "a restart is what it asked for");
    assert_eq!(logs.count("FAKE"), 0, "a value reached the log");
}

#[tokio::test]
async fn a_start_without_its_required_secrets_holds_until_they_are_delivered_and_then_goes_on() {
    let (logs, _guard) = Logs::capture();
    let cfg = config("startup-hold");
    let address = listen(&cfg);
    let tool = config("startup-hold-tool");
    let mut starting = spawn_startup(cfg);
    assert!(timeout(HOLDING, &mut starting).await.is_err(), "it did not hold for its chain key");
    assert_eq!(logs.critical("secret_missing"), 3, "one per field, each once");

    // A bundle without the chain key leaves it holding.
    let partial = bundle(1, &[("vendor_key", "FAKE-vendor")]);
    assert!(matches!(deliver(&tool, &address, partial).await, Reply::Sealed { .. }));
    assert!(timeout(HOLDING, &mut starting).await.is_err(), "it went on without its chain key");

    assert!(matches!(deliver(&tool, &address, full(2, "b")).await, Reply::Sealed { .. }));
    let (cfg, telemetry, delivered) = timeout(WAIT, starting).await.expect("it still holds").unwrap();
    let delivered = delivered.unwrap();
    assert_eq!(cfg.checkpoints.as_ref().unwrap().rpc_url.expose().unwrap().path(), "/v2/FAKE-chain-b");
    assert!(telemetry.pages_critical);
    assert_eq!(delivered.values.len(), 3);
    assert_eq!(
        logs.count("alert_resolved=\"secret_missing\""),
        3,
        "every field's alert resolved: {}",
        logs.count("alert_resolved")
    );
    assert_eq!(logs.count("FAKE"), 0, "a value reached the log");
}

#[tokio::test]
async fn a_start_without_a_vendor_key_or_a_webhook_runs_with_them_off() {
    let (logs, _guard) = Logs::capture();
    let cfg = config("startup-optional");
    sealed_earlier(&cfg, bundle(1, &[("chain_key", "FAKE-chain")]));
    let (cfg, telemetry, delivered) = timeout(WAIT, spawn_startup(cfg)).await.expect("it held").unwrap();
    let delivered = delivered.unwrap();
    let connection = &cfg.feeds.venues.as_ref().unwrap().connections[0];
    assert!(delivered.withholds(connection), "the vendor connection stays off");
    assert!(delivered.api_key(connection).unwrap().is_none(), "a key from somewhere other than the bundle");
    assert!(!telemetry.pages_critical, "no webhook, and no fallback for it");
    assert_eq!(logs.critical("secret_missing"), 2);
}

#[tokio::test]
async fn a_public_venue_is_never_withheld() {
    let cfg = config("startup-env");
    let delivered = Delivered::default();
    let mut connection = cfg.feeds.venues.as_ref().unwrap().connections[0].clone();
    connection.api_key_secret = None;
    assert!(!delivered.withholds(&connection), "no secret to lack");
    assert!(delivered.api_key(&connection).unwrap().is_none(), "a public venue has no key");
}

#[tokio::test]
async fn sealed_secrets_that_do_not_open_are_an_alert_and_a_hold_and_a_delivery_replaces_them() {
    let (logs, _guard) = Logs::capture();
    let cfg = config("startup-unreadable");
    let address = listen(&cfg);
    sealed_earlier(&cfg, full(7, "a"));
    let release = crate::tee::sealing::Release { launch_digest: RELEASE, config_sha256: CONFIG };
    std::fs::write(file_for(&sealing_dir(&cfg), &release), b"the host altered this").unwrap();
    let tool = config("startup-unreadable-tool");

    let mut starting = spawn_startup(cfg);
    assert!(timeout(HOLDING, &mut starting).await.is_err(), "it went on without its secrets");
    assert_eq!(logs.critical("secrets_unreadable"), 1);
    assert!(matches!(deliver(&tool, &address, full(1, "b")).await, Reply::Sealed { .. }));
    let (cfg, _, delivered) = timeout(WAIT, starting).await.expect("it still holds").unwrap();
    delivered.unwrap();
    assert!(logs.count("alert_resolved=\"secrets_unreadable\"") >= 1, "the alert was never resolved");
    assert_eq!(cfg.checkpoints.as_ref().unwrap().rpc_url.expose().unwrap().path(), "/v2/FAKE-chain-b");
}

// A key the attested config does not name, which is all a host has. A retired key the config still
// names does end a hold, by design: no log can be authenticated yet, and a bundle cannot change which
// provider is asked (`crate::secrets`).
#[tokio::test]
async fn a_stranger_cannot_end_the_hold() {
    let cfg = config("startup-stranger");
    let address = listen(&cfg);
    let mut starting = spawn_startup(cfg);
    let tool = config("startup-stranger-tool");
    let stranger = k256::ecdsa::SigningKey::from_slice(&[0x77; 32]).unwrap();
    let mut stream = timeout(WAIT, async {
        loop {
            match TcpStream::connect(&address).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .unwrap();
    let nonce = [0x5c; 32];
    send(&mut stream, &Hello { nonce: hex::encode(nonce) }).await.unwrap();
    let offer: Offer = receive(&mut stream).await.unwrap();
    let release = crate::tee::sealing::Release { launch_digest: RELEASE, config_sha256: CONFIG };
    let domain = domain_of(&tool);
    let message = delivery::encrypt(
        domain,
        &release,
        &hex::decode(&offer.public_key).unwrap(),
        &nonce,
        &full(1, "host").sign(domain, &stranger),
    )
    .unwrap();
    send(&mut stream, &message).await.unwrap();
    let reply: Reply = receive(&mut stream).await.unwrap();
    assert!(matches!(reply, Reply::Refused { .. }));
    assert!(timeout(HOLDING, &mut starting).await.is_err(), "a stranger's bundle ended the hold");
    starting.abort();
}

#[tokio::test]
async fn a_config_that_names_no_secret_starts_as_it_did() {
    let mut cfg = crate::test_support::dev_config();
    let mut telemetry = telemetry();
    let delivered = startup(&mut cfg, &mut telemetry, &KeyStart::default()).await.unwrap();
    assert!(delivered.listener.is_none() && delivered.store.is_none() && delivered.values.is_empty());
}

#[tokio::test]
async fn a_mock_start_that_names_secrets_needs_its_stand_in_measurement() {
    let mut cfg = config("startup-no-digest");
    let mut telemetry = telemetry();
    let Err(err) = startup(&mut cfg, &mut telemetry, &KeyStart::default()).await else { panic!("it started") };
    assert!(format!("{err:#}").contains("--mock-launch-digest"), "{err:#}");
}

#[tokio::test]
async fn a_delivered_webhook_that_is_no_longer_acceptable_leaves_its_route_off_with_an_alert() {
    // An unacceptable URL cannot be delivered; this is the seal an older build wrote.
    let (logs, _guard) = Logs::capture();
    let cfg = config("startup-bad-webhook");
    sealed_earlier(&cfg, bundle(1, &[("chain_key", "FAKE-chain"), ("critical_url", "http://hooks.example/FAKE-hook")]));
    let (_, telemetry, delivered) = timeout(WAIT, spawn_startup(cfg)).await.expect("it held").unwrap();
    delivered.unwrap();
    assert!(!telemetry.pages_critical);
    assert_eq!(logs.critical("secret_unusable"), 1);
    assert_eq!(logs.count("FAKE"), 0, "a value reached the log");
}

/// `config(name)`'s data beside each other under this test's directory, so a node can start on it.
fn startable(name: &str) -> NodeConfig {
    let mut cfg = config(name);
    let data = crate::test_support::scratch_dir(&format!("{name}-data"));
    cfg.node.log_dir = data.join("log");
    cfg.alerts.outbox.dir = data.join("alerts");
    cfg
}

// A delivered chain key the provider refuses stops a start before anything can use it. The node must
// not power off at once: the delivery that repairs it needs the listener. Once it has one, or once
// the hold has passed, it exits, and the next start takes what it sealed.
#[tokio::test]
async fn a_start_that_fails_with_a_chain_key_named_takes_a_delivery_and_exits() {
    let (logs, _guard) = Logs::capture();
    // With mock custody, reading `[checkpoints]` fails at once, as a refused key does.
    let cfg = startable("startup-failed");
    sealed_earlier(&cfg, full(1, "a"));
    let (address, tool) = (listen(&cfg), config("startup-failed-tool"));
    let running = crate::node::run_with_cpu(cfg, telemetry(), None, start());
    tokio::pin!(running);
    assert!(timeout(HOLDING, &mut running).await.is_err(), "the node stopped, and the listener with it");
    assert_eq!(logs.critical("secrets_start_failed"), 1);

    let Reply::Sealed { receipt, .. } = deliver(&tool, &address, full(2, "b")).await else { panic!("refused") };
    assert_eq!((receipt.version, receipt.restart_required), (2, true), "what it started with is what failed");
    let outcome = timeout(WAIT, running).await.expect("it did not exit once it had its bundle");
    assert!(outcome.is_err(), "the failed start's error is what it exits with");
}

// A transient failure with no delivery in sight must not keep the node down until someone stops it.
#[tokio::test]
async fn a_start_that_fails_with_a_chain_key_named_exits_after_its_hold() {
    let cfg = startable("startup-failed-hold");
    sealed_earlier(&cfg, full(1, "a"));
    let outcome = timeout(WAIT, crate::node::run_with_cpu(cfg, telemetry(), None, start())).await;
    assert!(outcome.expect("the node held past its bound").is_err());
}

// No delivered key can be at fault, so a failed start ends as it always has, with no hold.
#[tokio::test]
async fn a_start_that_fails_with_no_key_in_its_chain_read_ends_at_once() {
    let (logs, _guard) = Logs::capture();
    let mut cfg = startable("startup-failed-no-key");
    // The same failed read, with the URL written out.
    cfg.checkpoints = Some(
        toml::from_str(
            "rpc_url = 'https://rpc.example/v2'\nrelayer = '0x0000000000000000000000000000000000000001'\n\
             retry_interval = '1s'\nrpc_timeout = '1s'",
        )
        .unwrap(),
    );
    let outcome = timeout(UNDER_THE_HOLD, crate::node::run_with_cpu(cfg, telemetry(), None, start())).await;
    assert!(outcome.expect("the node held").is_err());
    assert_eq!(logs.critical("secrets_start_failed"), 0);
}

/// A chain whose every answer is zeros: the vault follows no registry, and nothing later decodes.
async fn zeros_chain() -> std::net::SocketAddr {
    use axum::routing::post;
    use serde_json::{Value, json};
    let answer = |axum::Json(request): axum::Json<Value>| async move {
        axum::Json(json!({"jsonrpc":"2.0","id":request["id"],"result":format!("0x{}", "00".repeat(32))}))
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, axum::Router::new().fallback(post(answer))).await });
    address
}

// The key worked, so nothing after that read is a delivery's to repair: holding would only delay the
// failure a start has always ended on.
#[tokio::test]
async fn a_start_that_fails_after_its_chain_read_ends_at_once() {
    let (logs, _guard) = Logs::capture();
    let mut cfg = startable("startup-failed-after-read");
    sealed_earlier(&cfg, full(1, "a"));
    let chain = zeros_chain().await;
    cfg.checkpoints = Some(
        toml::from_str(&format!(
            "rpc_url = 'http://{chain}/v2/{{chain_key}}'\nrelayer = '0x0000000000000000000000000000000000000001'\n\
             retry_interval = '1s'\nrpc_timeout = '1s'",
        ))
        .unwrap(),
    );
    cfg.custody.evm = Some(
        toml::from_str(
            "providers = [{ name = 'a', rpc_url = 'http://127.0.0.1:1' }]\n\
             usdc = '0xc0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0'\nfinality_tag = 'safe'\n\
             withdrawal_finality_tag = 'finalized'\nconfirmations = 0\nstart_block = 0\nmax_block_range = 1000\n\
             poll_interval = '10ms'\nrpc_timeout = '1s'\nstall_critical_after = '1s'\n\
             finality_stalled_after = '1s'\ncursor_path = 'unused.json'\n",
        )
        .unwrap(),
    );
    let outcome = timeout(UNDER_THE_HOLD, crate::node::run_with_cpu(cfg, telemetry(), None, start())).await;
    assert!(outcome.expect("the node held").is_err());
    assert_eq!(logs.critical("secrets_start_failed"), 0);
    assert_eq!(logs.count("the node read the chain"), 1, "the failure must come after the read");
}

// A bundle sealed while the failed read was still out is the repair: the hold must not wait for another.
#[tokio::test]
async fn a_bundle_sealed_during_the_failed_read_ends_the_hold() {
    let cfg = config("startup-sealed-during");
    sealed_earlier(&cfg, full(1, "a"));
    let (cfg, _, delivered) = timeout(WAIT, spawn_startup(cfg)).await.expect("it held").unwrap();
    // No connection sealed this one, so there is no receipt to wait for.
    let delivered = Delivered { answered: None, ..delivered.unwrap() };
    assert_eq!(delivered.version, Some(1));
    delivered.store.as_ref().unwrap().replace(full(2, "b").sign(domain_of(&cfg), &operator_key())).unwrap();
    let mut listening = JoinSet::new();
    listening.spawn(std::future::pending::<anyhow::Result<()>>());
    let waited = wait_for_a_new_bundle(&delivered, &mut listening, Duration::from_secs(600));
    timeout(UNDER_THE_HOLD, waited).await.expect("it waited for a bundle it already had");
}

// The exit powers the guest off, so going before the receipt leaves the operator nothing to retry on.
#[tokio::test]
async fn the_hold_outlasts_the_receipt_of_the_bundle_that_ended_it() {
    let cfg = config("startup-receipt");
    sealed_earlier(&cfg, full(1, "a"));
    let (cfg, _, delivered) = timeout(WAIT, spawn_startup(cfg)).await.expect("it held").unwrap();
    let (answering, answered) = watch::channel(None);
    let delivered = Delivered { answered: Some(answered), ..delivered.unwrap() };
    delivered.store.as_ref().unwrap().replace(full(2, "b").sign(domain_of(&cfg), &operator_key())).unwrap();
    let mut listening = JoinSet::new();
    listening.spawn(std::future::pending::<anyhow::Result<()>>());
    let waited = wait_for_a_new_bundle(&delivered, &mut listening, Duration::from_secs(600));
    tokio::pin!(waited);
    assert!(timeout(NEW_BUNDLE_POLL * 3, &mut waited).await.is_err(), "it ended before the receipt");
    answering.send_replace(Some(2));
    timeout(UNDER_THE_HOLD, waited).await.expect("the receipt did not end it");
}

// A failed move is when a holding release has something to say: it needs the operator.
#[tokio::test]
async fn a_holding_start_raises_what_it_lacks_once_the_move_has_failed() {
    let (logs, _guard) = Logs::capture();
    let mut cfg = config("startup-move-failed");
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    let start = KeyStart { predecessor: Some(closed), ..start() };
    let mut telemetry = telemetry();
    let held = timeout(Duration::from_secs(1), startup(&mut cfg, &mut telemetry, &start)).await;
    assert!(held.is_err(), "it went on without its chain key");
    assert_eq!(logs.critical("secret_missing"), 3, "the chain key, the vendor key, and the webhook");
}

// The screening source's key is one a node holds for, and fills only its URL's place.
#[tokio::test]
async fn a_start_holds_for_a_screening_key_and_sets_it_on_its_endpoint() {
    let mut cfg = config("startup-screening");
    cfg.access = Some(
        toml::from_str(
            "source = '0x0000000000000000000000000000000000000001'\n\
             endpoint = 'https://source.example/v1/{screening_key}'\ntimeout = '2s'\nmax_age = '30s'\n\
             max_response_bytes = 4096\nmax_in_flight = 8\n",
        )
        .unwrap(),
    );
    let (address, tool) = (listen(&cfg), config("startup-screening-tool"));
    sealed_earlier(&cfg, full(1, "a"));
    let mut starting = spawn_startup(cfg);
    assert!(timeout(HOLDING, &mut starting).await.is_err(), "it went on without its screening key");

    let everything = crate::secrets::tests::bundle(
        2,
        &[
            ("chain_key", "FAKE-chain-b"),
            ("vendor_key", "FAKE-vendor-b"),
            ("critical_url", "https://hooks.example/FAKE-hook-b"),
            ("screening_key", "FAKE-screening-b"),
        ],
    );
    let Reply::Sealed { .. } = deliver(&tool, &address, everything).await else { panic!("refused") };
    let (cfg, _, delivered) = timeout(WAIT, starting).await.expect("it still holds").unwrap();
    delivered.unwrap();
    let endpoint = cfg.access.as_ref().unwrap().endpoint.expose().unwrap();
    assert_eq!(endpoint.as_str(), "https://source.example/v1/FAKE-screening-b");
}

// With nothing required outstanding, a predecessor that never answers does not keep the keys' handover
// waiting for ever: the start goes on, and says what it lacks.
#[tokio::test]
async fn a_start_gives_up_on_a_predecessor_it_cannot_reach() {
    let (logs, _guard) = Logs::capture();
    let mut cfg = config("startup-no-predecessor");
    cfg.checkpoints = None;
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    let start = KeyStart { predecessor: Some(closed), ..start() };
    let mut telemetry = telemetry();
    let delivered = timeout(WAIT, startup(&mut cfg, &mut telemetry, &start)).await.expect("it waited for ever");
    assert!(delivered.unwrap().values.is_empty());
    assert_eq!(logs.count("taking the running release's secrets failed"), 3);
    assert_eq!(logs.critical("secret_missing"), 2, "the vendor key and the webhook it lacks");
}

#[tokio::test]
async fn a_listener_goes_on_taking_deliveries_after_a_start_with_no_failure_in_it() {
    let cfg = config("startup-listener-runs");
    sealed_earlier(&cfg, full(1, "a"));
    let address = listen(&cfg);
    let tool = config("startup-listener-runs-tool");
    let (_cfg, _telemetry, delivered) = timeout(WAIT, spawn_startup(cfg)).await.expect("it held").unwrap();
    let delivered = delivered.unwrap();
    let serving = tokio::spawn(delivered.listener.unwrap().run());
    let Reply::Sealed { receipt, .. } = deliver(&tool, &address, full(2, "b")).await else { panic!("refused") };
    assert!(receipt.restart_required, "{receipt:?}");
    serving.abort();
}
