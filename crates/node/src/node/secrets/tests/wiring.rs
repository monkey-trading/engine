//! The delivery listener's wiring through a real start: a node whose log records a rotated
//! operator must refuse the key its attested config names and take the key the log records.
//! Every other test and scenario builds `Authority::Log` by hand, so only a start through
//! `run_with_cpu` fails if the wiring drops the switch to the log's key.

use std::path::Path;

use engine_core::auth::Operator;
use engine_core::command::{AdminCommand, Command};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::operator_bounds::NOTICE_MS;
use k256::ecdsa::SigningKey;
use tokio::sync::{broadcast, mpsc, watch};

use super::*;
use crate::admin::sign;
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::log::Manifest;
use crate::sequencer::{Request, Sequencer, SequencerConfig};
use crate::tee::EnclaveKey;
use crate::tee::keys::KeySet;
use crate::tee::sealing::KeyStore;
use crate::test_support::{dev_config, sample_around, scratch_dir, sequencer_config, wait_for};

// A node recovers a log whose operator rotated away from the key its config names: the start's
// wiring hands the delivery listener the log's authority, which refuses the configured key and
// takes the recorded one. Without the wiring the config's key seals for the node's whole life.
#[tokio::test]
async fn a_started_node_takes_deliveries_from_the_key_its_log_records_not_the_configured_one() {
    let data = scratch_dir("delivery-wiring-data");
    let (configured, rotated) = log_recording_a_rotated_key(&data.join("log")).await;

    let mut cfg = config("delivery-wiring");
    // Mock custody reads no vault, and without `[checkpoints]` a fresh start runs the whole way.
    cfg.checkpoints = None;
    cfg.node.log_dir = data.join("log");
    cfg.history.dir = data.join("history");
    cfg.alerts.outbox.dir = data.join("alerts");
    cfg.node.listen = "127.0.0.1:0".parse().unwrap();
    cfg.metrics.listen = "127.0.0.1:0".parse().unwrap();
    // The sampler's stand-ins answer nothing: nothing here needs a trusted sample, and no test
    // packet should leave the machine.
    for server in &mut cfg.time.servers {
        server.address = "127.0.0.1:1".into();
    }
    let (address, tool) = (listen(&cfg), config("delivery-wiring-tool"));
    // Genesis ends in a checkpoint the shared test key signed, so this start's sealed keys must
    // open as that key: a fresh key fails closed against the checkpoint.
    let sealing = cfg.tee.sealing.as_ref().unwrap();
    KeyStore::create(
        &sealing.dir,
        &MockSealer { launch_digest: RELEASE },
        CONFIG,
        0,
        KeySet::for_key(&EnclaveKey::test_key()).unwrap(),
    )
    .unwrap();
    // The alert layer must outlive the node: dropped, the router's queue closes and supervision
    // stops the node.
    let metrics = Arc::new(Metrics::new());
    let (_alert_layer, alerts) = crate::alerts::channel(crate::alerts::Routes::default(), metrics.clone());
    let telemetry = Telemetry { metrics, alerts, pages_critical: false };
    let running = crate::node::run_with_cpu(cfg, telemetry, None, start());
    tokio::pin!(running);

    // Until the start has handed the listener's authority to the log, the config's key still
    // seals; afterwards it is refused. Retrying the same bundle waits out that switch.
    let stale = bundle(1, &[("vendor_key", "FAKE-vendor-old"), ("critical_url", "https://hooks.example/FAKE-old")]);
    let refused = tokio::select! {
        outcome = &mut running => panic!("the node exited before its listener took the log's key: {outcome:?}"),
        refused = wait_for("the config's key refused", async {
            loop {
                match deliver_signed(&tool, &address, stale.clone(), &operator_key()).await {
                    Reply::Refused { reason } => break reason,
                    Reply::Sealed { .. } => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        }) => refused,
    };
    assert!(refused.contains("not the operator key in effect"), "{refused}");
    assert!(refused.contains(&configured.address.to_string()), "another key named: {refused}");

    // The key the log records seals at once, at a version above what the retries sealed.
    let fresh = bundle(2, &[("vendor_key", "FAKE-vendor-new"), ("critical_url", "https://hooks.example/FAKE-new")]);
    let Reply::Sealed { receipt, .. } = deliver_signed(&tool, &address, fresh, &rotated).await else {
        panic!("the log's key was refused")
    };
    assert_eq!(receipt.version, 2);
    // `running` drops here: the pin's future never returns, and dropping it is what stops the node.
}

/// Writes, in `dir`, a log that records a rotated operator: the configured key gives notice, the
/// notice runs, and a command under the new key makes it the recorded one. The sequencer is
/// aborted without a final checkpoint: the node under test seals fresh keys, and a checkpoint the
/// shared test key signed would fail closed against them.
async fn log_recording_a_rotated_key(dir: &Path) -> (Operator, SigningKey) {
    let clock = Arc::new(TestClock::at(1_792_530_000_000));
    let operator = dev_config().operator().unwrap();
    // No checkpoint may be signed: a periodic one follows every `checkpoint_every`-th command.
    let cfg = SequencerConfig {
        clock: clock.clone(),
        checkpoint_every: u64::MAX,
        ..sequencer_config(dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(
        cfg,
        Arc::new(EnclaveKey::test_key()),
        broadcast::channel(16).0,
        mpsc::unbounded_channel().0,
    )
    .unwrap();
    seq.activate_protocol().unwrap();
    if seq.needs_genesis() {
        seq.genesis(&dev_config().genesis).unwrap();
    }
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(seq.run(rx));

    let new_key = SigningKey::from_slice(&[0x55; 32]).unwrap();
    let new_address = eip712::address(new_key.verifying_key());
    submit(&tx, &operator, &clock, AdminCommand::RotateOperator { address: new_address }, &operator_key()).await;
    clock.advance(Duration::from_millis(NOTICE_MS as u64 + 1_000));
    // The advanced stamp needs a fresh sample before trusted time admits the next command.
    tx.send(Request::TimeSample(sample_around(clock.as_ref()))).await.unwrap();
    submit(&tx, &operator, &clock, AdminCommand::SetReduceOnly { market: None, reduce_only: true }, &new_key).await;
    // Both commands are flushed before their replies; aborting writes no final checkpoint.
    drop(tx);
    task.abort();
    (operator, new_key)
}

/// Sequences one operator-signed command and asserts the engine applied it. Each key signs its
/// one command at its request sequence's head.
async fn submit(
    tx: &mpsc::Sender<Request>,
    operator: &Operator,
    clock: &TestClock,
    command: AdminCommand,
    key: &SigningKey,
) {
    let signed = sign(key, operator, command, 0, clock.now_ms() + 150_000);
    let (reply, done) = tokio::sync::oneshot::channel();
    tx.send(Request::Submit { cmd: Command::SignedAdmin(Box::new(signed)), reply: Some(reply) }).await.unwrap();
    let events = done.await.unwrap();
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
}
