//! Refused startup must not create, checkpoint, discard, or modify persisted history/log bytes.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use engine_core::Fixed;
use engine_core::types::AccountId;

use crate::checkpoints::{CheckpointChain, Health, Poster, VaultState, View};
use crate::history::HistoryPlan;
use crate::log::CheckpointDomain;
use crate::node::preflight::finish_preflight;
use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, scratch_dir};

struct Chain {
    state: VaultState,
}
impl CheckpointChain for Chain {
    async fn state(&self, _: View) -> anyhow::Result<VaultState> {
        Ok(self.state.clone())
    }
    async fn post(&self, _: &crate::log::Checkpoint) -> anyhow::Result<()> {
        unreachable!()
    }
}

#[tokio::test]
async fn rejected_anchor_and_signer_preserve_missing_corrupt_and_wal_history_bytes() {
    for history_kind in 0..3 {
        for signer_mismatch in [false, true] {
            let root = scratch_dir(&format!("preflight-bytes-{history_kind}-{signer_mismatch}"));
            std::fs::create_dir_all(&root).unwrap();
            let mut cfg = dev_config();
            cfg.node.log_dir = root.join("log");
            cfg.history.dir = root.join("history");
            std::fs::create_dir_all(&cfg.node.log_dir).unwrap();
            std::fs::write(cfg.node.log_dir.join("manifest.jsonl"), b"{\"interrupted\":").unwrap();
            let mut conn = None;
            if history_kind > 0 {
                std::fs::create_dir_all(&cfg.history.dir).unwrap();
                let path = cfg.history.dir.join(crate::history::INDEX_FILE);
                if history_kind == 1 {
                    std::fs::write(path, b"corrupt diagnostic bytes").unwrap();
                } else {
                    let db = rusqlite::Connection::open(path).unwrap();
                    db.execute_batch("PRAGMA journal_mode=wal; CREATE TABLE diagnostic(value TEXT); INSERT INTO diagnostic VALUES('preserve WAL');").unwrap();
                    conn = Some(db);
                }
            }
            let before = files(&root);
            let key = EnclaveKey::generate();
            let poster = Poster {
                registry: None,
                chain: Chain {
                    state: VaultState {
                        height: 0,
                        signer: if signer_mismatch { AccountId([9; 20]) } else { AccountId(key.address()) },
                        checkpoint: Some((9, format!("0x{}", "ab".repeat(32)))),
                        balance: Fixed::ZERO,
                        deposits: 0,
                        deposited: Fixed::ZERO,
                        distributions_paid: Fixed::ZERO,
                    },
                },
                domain: CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) },
                signer: AccountId(key.address()),
                retry: Duration::from_millis(1),
                pending_deposits_critical_after: Duration::from_secs(1),
            };
            let plan = HistoryPlan::open(&cfg.history).unwrap();
            assert!(finish_preflight(plan, Some((&poster, None)), None).await.is_err());
            assert_eq!(files(&root), before, "history kind {history_kind}, signer mismatch {signer_mismatch}");
            drop(conn);
        }
    }
}

#[tokio::test]
async fn replay_valid_truncated_log_cannot_discard_a_newer_valid_history_index() {
    let root = scratch_dir("preflight-truncated-valid-index");
    std::fs::create_dir_all(&root).unwrap();
    let mut cfg = dev_config();
    let full = root.join("full-log");
    cfg.history.dir = root.join("history");
    let node = crate::test_support::TestSequencer::start(&full, 1024);
    let history = crate::test_support::TestHistory::start(&node, &full, &cfg.history.dir).await.unwrap();
    node.submit(crate::test_support::deposit(100, "startup-boundary")).await;
    node.stop().await;
    history.finish().await.unwrap();
    let entries = crate::test_support::log_entries(&full);
    let anchor = entries
        .iter()
        .rev()
        .find_map(|entry| if let crate::log::LogEntry::Checkpoint(cp) = entry { Some(cp.clone()) } else { None })
        .unwrap();
    cfg.node.log_dir = root.join("short-log");
    std::fs::create_dir_all(&cfg.node.log_dir).unwrap();
    let short = entries
        .iter()
        .filter(|entry| match entry {
            crate::log::LogEntry::Command(command) => command.sequenced.seq < anchor.seq,
            crate::log::LogEntry::Checkpoint(cp) => cp.seq < anchor.seq,
            _ => true,
        })
        .map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap()))
        .collect::<String>();
    std::fs::write(cfg.node.log_dir.join("00000000000000000000.jsonl"), short).unwrap();
    let before = files(&root);
    let key = EnclaveKey::test_key();
    let mut found = None;
    let (_log, plan) = crate::node::read_log_and_history_observe(&cfg, Some(AccountId(key.address())), None, |entry| {
        if let crate::log::LogEntry::Checkpoint(cp) = entry
            && cp.seq == anchor.seq
        {
            found = Some(cp.clone());
        }
    })
    .unwrap();
    let poster = Poster {
        registry: None,
        chain: Chain {
            state: VaultState {
                height: 0,
                signer: AccountId(key.address()),
                checkpoint: Some((anchor.seq, anchor.root)),
                balance: Fixed::from_int(100),
                deposits: 1,
                deposited: Fixed::from_int(100),
                distributions_paid: Fixed::ZERO,
            },
        },
        domain: anchor.domain.unwrap(),
        signer: AccountId(key.address()),
        retry: Duration::from_millis(1),
        pending_deposits_critical_after: Duration::from_secs(1),
    };
    assert!(finish_preflight(plan, Some((&poster, found.as_ref())), None).await.is_err());
    assert_eq!(files(&root), before);
}

fn files(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(files(&path));
        } else {
            result.insert(path.clone(), std::fs::read(path).unwrap());
        }
    }
    result
}

#[tokio::test]
async fn wrong_recovery_deployment_preserves_history_wal_and_log_bytes() {
    let root = scratch_dir("preflight-recovery-deployment");
    let mut cfg = dev_config();
    cfg.node.log_dir = root.join("log");
    cfg.history.dir = root.join("history");
    let node = crate::test_support::TestSequencer::start(&cfg.node.log_dir, 1);
    node.stop().await;
    std::fs::create_dir_all(&cfg.history.dir).unwrap();
    let db = rusqlite::Connection::open(cfg.history.dir.join(crate::history::INDEX_FILE)).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=wal; CREATE TABLE diagnostic(value TEXT); INSERT INTO diagnostic VALUES('keep');",
    )
    .unwrap();
    for wrong_operator in [false, true] {
        let (_guard, mut alerts) = crate::test_support::capture_alerts();
        let before = files(&root);
        let (log, plan) = crate::node::read_log_and_history_observe(
            &cfg,
            Some(AccountId(EnclaveKey::test_key().address())),
            None,
            |_| {},
        )
        .unwrap();
        let mut seq_cfg = crate::test_support::sequencer_config(
            &cfg.node.log_dir,
            tokio::sync::watch::channel(crate::log::Manifest::default()).0,
        );
        if wrong_operator {
            seq_cfg.operator.chain_id += 1;
        } else {
            seq_cfg.client_auth = engine_core::auth::ClientAuth::Eip712 { chain_id: 1, vault: AccountId([7; 20]) };
        }
        let error =
            finish_preflight::<Chain>(plan, None, Some((&log, &seq_cfg, AccountId(EnclaveKey::test_key().address()))))
                .await
                .err()
                .unwrap();
        assert!(error.to_string().contains(if wrong_operator { "operator" } else { "client auth" }));
        assert_eq!(files(&root), before);
        let alert = alerts.try_recv().expect("recovery refusal raises Critical");
        assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("log_recovery_failed", "ERROR"));
    }
    drop(db);
}

#[tokio::test]
async fn incomplete_genesis_and_missing_checkpoint_service_preserve_history_bytes() {
    use crate::log::{LogEntry, Manifest};
    use crate::sequencer::{Sequencer, SequencerConfig};
    use crate::test_support::{sequencer_config, trusted_now};
    use tokio::sync::{broadcast, mpsc, watch};

    for incomplete_genesis in [false, true] {
        let (_guard, mut alerts) = crate::test_support::capture_alerts();
        let root = scratch_dir(&format!("preflight-pure-genesis-{incomplete_genesis}"));
        let mut cfg = dev_config();
        cfg.node.log_dir = root.join("log");
        cfg.history.dir = root.join("history");
        let (work, mut queued) = mpsc::channel(64);
        let seq_cfg = SequencerConfig {
            checkpoints: Some(work),
            checkpoint_every: 1,
            ..sequencer_config(&cfg.node.log_dir, watch::channel(Manifest::default()).0)
        };
        let mut seq = Sequencer::recover(
            seq_cfg,
            std::sync::Arc::new(EnclaveKey::test_key()),
            broadcast::channel(128).0,
            mpsc::unbounded_channel().0,
        )
        .unwrap();
        seq.activate_protocol().unwrap();
        seq.genesis(&cfg.genesis).unwrap();
        seq.trust(trusted_now()).unwrap();
        while queued.try_recv().is_ok() {}
        // Drive approvals through the public sequencer request path.
        let (tx, rx) = mpsc::channel(64);
        let task = tokio::spawn(seq.run(rx));
        tx.send(crate::sequencer::Request::CheckpointStatus {
            posted: None,
            health: Health::Healthy,
            lifecycle: None,
        })
        .await
        .unwrap();
        for cmd in [crate::test_support::deposit(100, "preflight-pure"), crate::test_support::withdraw(40)] {
            let (reply, done) = tokio::sync::oneshot::channel();
            tx.send(crate::sequencer::Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
            done.await.unwrap();
        }
        drop(tx);
        task.await.unwrap().unwrap();
        if incomplete_genesis {
            let mut entries = crate::test_support::log_entries(&cfg.node.log_dir);
            let first = entries.iter().position(|entry| matches!(entry, LogEntry::Command(command) if matches!(command.sequenced.cmd, engine_core::command::Command::Admin(engine_core::command::AdminCommand::SetParams(_))))).unwrap();
            entries.truncate(first + 1);
            std::fs::remove_dir_all(&cfg.node.log_dir).unwrap();
            std::fs::create_dir_all(&cfg.node.log_dir).unwrap();
            let bytes =
                entries.iter().map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap())).collect::<String>();
            std::fs::write(cfg.node.log_dir.join("00000000000000000000.jsonl"), bytes).unwrap();
        }
        std::fs::create_dir_all(&cfg.history.dir).unwrap();
        let db = rusqlite::Connection::open(cfg.history.dir.join(crate::history::INDEX_FILE)).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=wal; CREATE TABLE diagnostic(value TEXT); INSERT INTO diagnostic VALUES('preserve');",
        )
        .unwrap();
        let before = files(&root);
        let (log, plan) = crate::node::read_log_and_history_observe(
            &cfg,
            Some(AccountId(EnclaveKey::test_key().address())),
            None,
            |_| {},
        )
        .unwrap();
        let seq_cfg = sequencer_config(&cfg.node.log_dir, watch::channel(Manifest::default()).0);
        let error =
            finish_preflight::<Chain>(plan, None, Some((&log, &seq_cfg, AccountId(EnclaveKey::test_key().address()))))
                .await
                .err()
                .unwrap();
        assert!(
            error.to_string().contains(if incomplete_genesis { "genesis" } else { "checkpoint service" }),
            "{error:#}"
        );
        assert_eq!(files(&root), before);
        let alert = alerts.try_recv().expect("pure recovery refusal raises Critical");
        assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("log_recovery_failed", "ERROR"));
        drop(db);
    }
}

struct ReservedChain {
    state: VaultState,
    checkpoint: crate::log::Checkpoint,
}
impl CheckpointChain for ReservedChain {
    fn reserved_checkpoint(&self) -> anyhow::Result<Option<crate::log::Checkpoint>> {
        Ok(Some(self.checkpoint.clone()))
    }
    async fn state(&self, _: View) -> anyhow::Result<VaultState> {
        Ok(self.state.clone())
    }
    async fn post(&self, _: &crate::log::Checkpoint) -> anyhow::Result<()> {
        unreachable!()
    }
}

#[tokio::test]
async fn reserved_signature_missing_from_recovered_log_refuses_before_history_mutation() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let root = scratch_dir("preflight-reserved-signature");
    let mut cfg = dev_config();
    cfg.node.log_dir = root.join("log");
    cfg.history.dir = root.join("history");
    std::fs::create_dir_all(&cfg.node.log_dir).unwrap();
    std::fs::create_dir_all(&cfg.history.dir).unwrap();
    let db = rusqlite::Connection::open(cfg.history.dir.join(crate::history::INDEX_FILE)).unwrap();
    db.execute_batch("PRAGMA journal_mode=wal; CREATE TABLE diagnostic(value TEXT); INSERT INTO diagnostic VALUES('keep reserved evidence');").unwrap();
    let key = EnclaveKey::test_key();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
    let parent = crate::log::CheckpointParent::default();
    let checkpoint = crate::log::Checkpoint {
        seq: 10,
        root: format!("0x{}", "12".repeat(32)),
        signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, 10, &[0x12; 32], parent),
        signer: key.address_hex(),
        parent: Some(parent),
        domain: Some(domain),
        deferred_withdrawals: true,
    };
    let poster = Poster {
        registry: None,
        chain: ReservedChain {
            state: VaultState {
                height: 0,
                signer: AccountId(key.address()),
                checkpoint: None,
                balance: Fixed::ZERO,
                deposits: 0,
                deposited: Fixed::ZERO,
                distributions_paid: Fixed::ZERO,
            },
            checkpoint,
        },
        domain,
        signer: AccountId(key.address()),
        retry: Duration::from_millis(1),
        pending_deposits_critical_after: Duration::from_secs(1),
    };
    let before = files(&root);
    let (log, plan) =
        crate::node::read_log_and_history_observe(&cfg, Some(AccountId(key.address())), Some(parent), |_| {}).unwrap();
    let seq_cfg = crate::sequencer::SequencerConfig {
        checkpoints: Some(tokio::sync::mpsc::channel(64).0),
        ..crate::test_support::sequencer_config(
            &cfg.node.log_dir,
            tokio::sync::watch::channel(crate::log::Manifest::default()).0,
        )
    };
    let error = finish_preflight(
        plan,
        Some((&poster, None)),
        Some((&log, &seq_cfg, AccountId(EnclaveKey::test_key().address()))),
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("absent"), "{error:#}");
    let alert = alerts.try_recv().expect("reservation refusal raises Critical");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("checkpoint_startup_failed", "ERROR"));
    assert_eq!(files(&root), before);
    drop(db);
}

#[tokio::test]
async fn a_genesis_no_current_config_can_name_raises_critical_and_preserves_bytes() {
    use crate::sequencer::fixture_tests::{geneses_no_config_can_name, old_genesis_log};
    for (shape, version, genesis, reason) in geneses_no_config_can_name() {
        let root = scratch_dir(&format!("preflight-unnameable-{shape}"));
        let mut cfg = dev_config();
        cfg.node.log_dir = root.join("log");
        cfg.history.dir = root.join("history");
        old_genesis_log(&cfg.node.log_dir, &cfg.client_auth().unwrap(), version, genesis, Vec::new());
        let (_guard, mut alerts) = crate::test_support::capture_alerts();
        let before = files(&root);
        let signer = AccountId(EnclaveKey::test_key().address());
        let (log, plan) = crate::node::read_log_and_history_observe(&cfg, Some(signer), None, |_| {}).unwrap();
        let seq_cfg = crate::test_support::sequencer_config(
            &cfg.node.log_dir,
            tokio::sync::watch::channel(crate::log::Manifest::default()).0,
        );
        let error = finish_preflight::<Chain>(plan, None, Some((&log, &seq_cfg, signer))).await.err().unwrap();
        let error = format!("{error:#}");
        assert!(error.contains("no current config can name") && error.contains(reason), "{shape}: {error}");
        assert_eq!(files(&root), before, "{shape}");
        let alert = alerts.try_recv().expect("recovery refusal raises Critical");
        assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("log_recovery_failed", "ERROR"));
    }
}

mod origin;
