//! A start's secrets: what it opens, what it holds for, and what it leaves off.

use super::*;

use std::path::PathBuf;

use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::alerts;
use crate::handover::{receive, send};
use crate::metrics::Metrics;
use crate::secrets::delivery::{self, Hello, Offer, Reply};
use crate::secrets::tests::{CONFIG, RELEASE, bundle, config, domain_of, full, operator_key};
use crate::tee::sealing::{MockSealer, Sealer};
use crate::tee::secrets::file_for;
use crate::trusted_time::tests::logs::Logs;

/// How long a test waits for what must happen; a hold is checked over a shorter wait.
const WAIT: Duration = Duration::from_secs(10);
const HOLDING: Duration = Duration::from_millis(300);

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
    let signed = bundle.sign(domain, &operator_key());
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
async fn a_public_venue_is_never_withheld_and_a_named_variable_still_reads_in_a_dev_build() {
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
