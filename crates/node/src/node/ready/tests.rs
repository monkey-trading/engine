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
    let follows = |log: &std::path::Path| {
        log_entries(log)
            .into_iter()
            .any(|entry| matches!(entry, LogEntry::Command(command) if command.sequenced.seq == last + 1))
    };
    crate::test_support::wait_for("the first command in the log", async {
        while !follows(&log) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
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

/// A member's start checks its replica's and its genesis's staking config against the contract before it connects to
/// anything else: here the checkpoint endpoint and every time server are unreachable, the contract's
/// providers answer, and the contract holds another epoch than the genesis record, so the refusal is
/// `staking_terms_mismatch` and not a connection failure.
#[tokio::test]
async fn a_member_whose_staking_config_the_contract_does_not_hold_is_refused_before_it_connects() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let data = scratch_dir("ready-staking-mismatch");
    let log = data.join("log");
    let cfg = unreachable(&data, &log);
    write_log(&cfg).await;
    let key = EnclaveKey::test_key();
    let signer = AccountId(key.address());
    let domain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };
    let follower = Follower::open(&log, 4, domain, signer, [signer].into(), None, |_| {}).unwrap();

    // The log's and the genesis's one-day epoch is not the contract's hour.
    let cfg = served_by_contract(cfg, 3_600).await;
    let err = read_member(&cfg, signer, &follower).await.expect("a start whose staking config differs read on");
    assert!(format!("{err:#}").starts_with("the log: epoch 86400000 ms differs"), "{err:#}");
    let alert = alerts.try_recv().expect("the refusal raises an alert");
    assert_eq!(alert["alert"], "staking_terms_mismatch");

    // Holding the contract's own values, the same start passes the check and fails on what it reads next.
    let cfg = served_by_contract(cfg, 86_400).await;
    let err = read_member(&cfg, signer, &follower).await.expect("the checkpoint endpoint is unreachable");
    assert!(!format!("{err:#}").contains("staking"), "{err:#}");
}

/// `Ready::read` for `signer` over `follower`'s replica, and the error it stops on.
async fn read_member(cfg: &NodeConfig, signer: AccountId, follower: &Follower) -> Option<anyhow::Error> {
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
    let start = crate::node::KeyStart { mock_launch_digest: Some(DIGEST), ..Default::default() };
    let sample = std::future::ready(Err(anyhow::anyhow!("the start went on to sample time")));
    Ready::read(cfg, &start, &Metrics::new(), signer, follower.replica(), (&turn, 0), sample).await.err()
}

/// `cfg` with its custody providers on a stand-in whose staking contract holds `epoch_secs` and the
/// genesis's cooldown.
async fn served_by_contract(mut cfg: NodeConfig, epoch_secs: u64) -> NodeConfig {
    let evm = cfg.custody.evm.as_mut().unwrap();
    let rpc = crate::test_support::staking_rpc(evm.usdc, epoch_secs, 2_419_200).await;
    for (provider, path) in evm.providers.iter_mut().zip(["a", "b", "c"]) {
        let section = format!("name = '{path}'\nrpc_url = '{rpc}/{path}'");
        provider.rpc_url = toml::from_str::<RpcProviderSection>(&section).unwrap().rpc_url;
    }
    cfg
}

/// A start that is not a member's reads the staking contract the config names before it serves, and
/// refuses a genesis staking config it cannot check: here the contract's providers are unreachable.
#[tokio::test]
async fn a_start_that_is_not_a_members_checks_its_genesis_staking_config_against_the_contract() {
    let data = scratch_dir("ready-staking-start");
    let log = data.join("log");
    let mut cfg = unreachable(&data, &log);
    cfg.tee.use_published_mock_key = true;
    cfg.tee.sealing = None;
    cfg.checkpoints = None;
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
    let keys = crate::node::KeyStart::default();
    let mut chain_read = false;
    let serving =
        crate::node::serve_node(&cfg, metrics, &mut routing, None, &keys, &delivered, &mut chain_read, None, None);
    let outcome = tokio::time::timeout(Duration::from_secs(60), serving).await.expect("the start refused, not served");
    let Err(err) = outcome else { panic!("a start whose staking contract cannot be read served") };
    assert!(format!("{err:#}").contains("the config's genesis: cannot read the staking contract"), "{err:#}");
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
    // The staking contract nothing answers for, and a genesis that records a config for it: a member's
    // start must not read it, since its replica was checked against it before it connected.
    evm.staking = Some(
        toml::from_str(
            "contract = '0x00000000000000000000000000000000000000dd'\ncursor_path = 'staking.json'\n\
             [registry]\ncontract = '0x00000000000000000000000000000000000000ee'",
        )
        .unwrap(),
    );
    evm.staking.as_mut().unwrap().cursor_path = data.join("staking.json");
    cfg.genesis.staking = Some(engine_core::staking::StakingConfig {
        liquidity_floor: Fixed::from_int(1_000),
        epoch_ms: 86_400_000,
        start_ms: crate::clock::now_ms() + 30 * 86_400_000,
        cooldown_ms: 2_419_200_000,
    });
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
