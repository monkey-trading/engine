//! A member's start from its replica reads nothing from the chain and samples no trusted time: run with
//! every chain endpoint and every time server unreachable, the node sequences its first command under
//! what the member read before connecting.

use std::time::Duration;

use engine_core::Fixed;

use super::*;
use crate::checkpoints::{CheckpointRpc, Observation, VaultState, View};
use crate::config::{CustodyProvider, FinalityTag, RpcProviderSection};
use crate::log::{CheckpointDomain, CheckpointParent, LogEntry};
use crate::node::member::Replica;
use crate::node::secrets::Delivered;
use crate::standby::Follower;
use crate::stats::Stats;
use crate::tee::EnclaveKey;
use crate::tee::keys::KeySet;
use crate::tee::sealing::MockSealer;
use crate::test_support::{dev_config, log_entries, sample_around, scratch_dir};

/// The stand-in measurement this start runs under.
const DIGEST: [u8; 48] = [0x6d; 48];
/// A loopback address nothing answers on.
const DEAD: &str = "http://127.0.0.1:9";

#[tokio::test]
async fn a_member_starts_from_its_replica_without_reading_the_chain_or_sampling_time() {
    let data = scratch_dir("ready-start");
    let log = data.join("log");
    let cfg = unreachable(&data, &log);
    write_log(&cfg).await;
    let key = EnclaveKey::test_key();
    let signer = AccountId(key.address());
    let domain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };
    let mut history = crate::history::HistoryPlan::open(&cfg.history).unwrap();
    let stats = Stats::new(&cfg).unwrap();
    let follower =
        Follower::open(&log, 4, domain, signer, [signer].into(), Some(stats.clone()), |entry| history.observe(entry))
            .unwrap();
    let last = follower.replica().last_seq().unwrap();
    let anchor = follower.replica().last_checkpoint().cloned().unwrap();
    let sealer = MockSealer { launch_digest: DIGEST };
    let sealing = cfg.tee.sealing.as_ref().unwrap();
    let store = KeyStore::create(&sealing.dir, &sealer, cfg.file_sha256, 0, KeySet::for_key(&key).unwrap()).unwrap();
    let ready = Ready {
        setup: Some(setup(&cfg, &data, &anchor, signer)),
        anchor: Some(anchor),
        emission_budget: None,
        heads: None,
        generations: BTreeMap::new(),
        sample: sample_around(&crate::clock::SystemClock),
        certificates: None,
        promoted: None,
    };
    let moved = Moved { after: last, entries: 0, bytes: 0 };
    let marks = Default::default();
    let start = Start { replica: Replica { follower, stats, history, marks }, store, ready, moved };

    let metrics = Arc::new(Metrics::new());
    let (_layer, alerts) = crate::alerts::channel(crate::alerts::Routes::default(), metrics.clone());
    let mut routing = alerts.spawn(cfg.alerts.episode_quiet, &cfg.alerts.outbox).await.unwrap();
    let delivered = Delivered {
        values: Default::default(),
        version: None,
        answered: None,
        chain_read_keyed: false,
        listener: None,
        authority: None,
        store: None,
    };
    let keys = crate::node::KeyStart { mock_launch_digest: Some(DIGEST), ..Default::default() };
    let mut chain_read = false;
    let running = crate::node::serve_node(
        &cfg,
        metrics.clone(),
        &mut routing,
        None,
        &keys,
        &delivered,
        &mut chain_read,
        Some(Box::new(start)),
        None,
    );
    tokio::pin!(running);
    tokio::select! {
        outcome = &mut running => panic!("the node stopped before its first command: {outcome:?}"),
        () = crate::test_support::wait_for("the first command", async {
            loop {
                if metrics.commands.get() > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }) => {}
    }
    assert_eq!(metrics.full_replays.get(), 0, "the start replayed the log");
    let next = log_entries(&log).into_iter().find_map(|entry| match entry {
        LogEntry::Command(command) if command.sequenced.seq == last + 1 => Some(command),
        _ => None,
    });
    assert!(next.is_some(), "the first command does not follow the replica's last");
}

/// The sealed certificate opens before anything is read from the chain, so a store the host altered
/// refuses the turn before the member connects and the incumbent stops: here every chain endpoint is
/// unreachable, and the refusal names the store.
#[tokio::test]
async fn a_certificate_store_that_does_not_open_refuses_before_the_member_connects() {
    let data = scratch_dir("ready-certificates");
    let log = data.join("log");
    let mut cfg = unreachable(&data, &log);
    write_log(&cfg).await;
    let acme =
        "[acme]\ndomain = 'api.engine.test'\ndirectory_url = 'https://ca.test/dir'\ncontact = 'mailto:ops@engine.test'";
    cfg.api.tls = Some(toml::from_str(acme).unwrap());
    let start = crate::node::KeyStart { mock_launch_digest: Some(DIGEST), ..Default::default() };
    let store = crate::node::keys::certificates(&cfg, &start).unwrap().expect("a store with [api.tls.acme]");
    store.replace("api.engine.test", &[0x7e; 32], "https://ca.test/dir", &[vec![0x5a; 48].into()]).unwrap();
    let path = crate::tee::certificates::file_for(&cfg.tee.sealing.as_ref().unwrap().dir, &store.release());
    let file = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, file.replace("\"format\":1", "\"format\":2")).unwrap();

    let key = EnclaveKey::test_key();
    let signer = AccountId(key.address());
    let domain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };
    let follower = Follower::open(&log, 4, domain, signer, [signer].into(), None, |_| {}).unwrap();
    let sample = std::future::ready(Err(anyhow::anyhow!("the start sampled time after its certificate failed")));
    let turn = crate::checkpoints::ScheduledTurn {
        turn: 1,
        boundary: 0,
        candidate: signer,
        admin_key: signer,
        release: [0; 32],
        generation: 0,
        set: 1,
        source: None,
    };
    let read = Ready::read(&cfg, &start, &Metrics::new(), signer, follower.replica(), (&turn, 0), sample).await;
    let err = read.err().expect("a start whose certificate does not open read on");
    assert!(format!("{err:#}").contains("has format 2"), "{err:#}");
}

// ── Helpers ──

/// Writes the log `cfg` names under its genesis, which credits no house capital: a start with
/// `[checkpoints]` proves every credit against custody, and this log needs none. It ends at a
/// checkpoint the test key signed.
async fn write_log(cfg: &NodeConfig) {
    let (manifest, _) = tokio::sync::watch::channel(crate::log::Manifest::default());
    let seq_cfg = crate::sequencer::SequencerConfig {
        genesis: crate::sequencer::genesis_commands(&cfg.genesis),
        ..crate::test_support::sequencer_config(&cfg.node.log_dir, manifest)
    };
    let (stream, withdrawals) = (tokio::sync::broadcast::channel(16).0, tokio::sync::mpsc::unbounded_channel().0);
    let key = Arc::new(EnclaveKey::test_key());
    let mut seq = crate::sequencer::Sequencer::recover(seq_cfg, key, stream, withdrawals).unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&cfg.genesis).unwrap();
    seq.trust(sample_around(&crate::clock::SystemClock)).unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let task = tokio::spawn(seq.run(rx));
    for _ in 0..6 {
        let (reply, applied) = tokio::sync::oneshot::channel();
        let cmd = engine_core::command::Command::Tick;
        tx.send(crate::sequencer::Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
        applied.await.unwrap();
    }
    drop(tx);
    task.await.unwrap().unwrap();
}

/// A member's config whose chain endpoints and time servers nothing answers on: with `[checkpoints]`,
/// EVM custody, sealing and a handover listener, on free ports and in `data`, over the log in `log`.
fn unreachable(data: &std::path::Path, log: &std::path::Path) -> NodeConfig {
    let mut cfg = dev_config();
    cfg.genesis.house_capital = Fixed::ZERO;
    cfg.node.log_dir = log.to_owned();
    cfg.history.dir = data.join("history");
    cfg.alerts.outbox.dir = data.join("alerts");
    cfg.node.listen = "127.0.0.1:0".parse().unwrap();
    cfg.metrics.listen = "127.0.0.1:0".parse().unwrap();
    for server in &mut cfg.time.servers {
        server.address = "127.0.0.1:1".into();
    }
    cfg.tee.use_published_mock_key = false;
    cfg.tee.sealing = Some(
        toml::from_str(&format!(
            "dir = {:?}\n[handover]\nlisten = '127.0.0.1:0'",
            data.join("keys").display().to_string()
        ))
        .unwrap(),
    );
    let anvil: NodeConfig =
        toml::from_str(&std::fs::read_to_string(crate::test_support::ANVIL_CONFIG).unwrap()).unwrap();
    let mut evm = anvil.custody.evm.unwrap();
    evm.providers = (0..3)
        .map(|n| toml::from_str::<RpcProviderSection>(&format!("name = 'dead-{n}'\nrpc_url = '{DEAD}'")).unwrap())
        .collect();
    evm.cursor_path = data.join("custody.json");
    evm.staking = None;
    cfg.custody.provider = CustodyProvider::Evm;
    cfg.custody.evm = Some(evm);
    cfg.checkpoints = Some(
        toml::from_str(&format!(
            "rpc_url = '{DEAD}'\nrelayer = '0x0000000000000000000000000000000000000001'\nretry_interval = '1s'\nrpc_timeout = '1s'"
        ))
        .unwrap(),
    );
    cfg
}

/// The checkpoint setup a member reads before it connects, here built by hand: the vault's finalized
/// checkpoint is `anchor`, and its client talks to nothing.
fn setup(cfg: &NodeConfig, data: &std::path::Path, anchor: &crate::log::Checkpoint, signer: AccountId) -> Setup {
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() };
    let section = cfg.checkpoints.as_ref().unwrap();
    let usdc = cfg.custody.evm.as_ref().unwrap().usdc;
    let chain = CheckpointRpc::new(section, domain, usdc, FinalityTag::Finalized, 0, data, signer).unwrap();
    let parent = CheckpointParent { has_checkpoint: true, seq: anchor.seq, root: anchor.root.parse().unwrap() };
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let state = VaultState {
        height: 1,
        signer,
        checkpoint: Some((anchor.seq, anchor.root.clone())),
        balance: Fixed::ZERO,
        deposits: 0,
        deposited: Fixed::ZERO,
        distributions_paid: Fixed::ZERO,
    };
    Setup {
        poster: crate::test_support::poster(chain, domain, signer),
        tx,
        rx,
        approvals: Default::default(),
        observation: Observation { view: View::Finalized, state, lifecycle: None },
        parent,
        posted_seq: Some(anchor.seq),
        registry_operator: None,
    }
}
