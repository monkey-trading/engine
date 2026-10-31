//! Older funded bootstrap roots require an exact prefix proof before writable startup.

use std::sync::Arc;

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712::B256;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::finish_preflight;
use super::tests::files;
use crate::checkpoints::CheckpointChain;
use crate::config::NodeConfig;
use crate::log::{Checkpoint, CheckpointDomain, CheckpointParent, LogEntry, Manifest, SegmentedLog};
use crate::sequencer::{Request, Sequencer, SequencerConfig, replay_path_with_trust};
use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, log_entries, scratch_dir, sequencer_config, trusted_now};

struct NoChain;
impl CheckpointChain for NoChain {
    async fn state(&self, _: crate::checkpoints::View) -> anyhow::Result<crate::checkpoints::VaultState> {
        panic!("pure recovery refusal precedes custody RPC")
    }
    async fn post(&self, _: &Checkpoint) -> anyhow::Result<()> {
        panic!("pure recovery refusal never posts")
    }
}

#[tokio::test]
async fn funded_unscanned_anchor_refuses_startup_without_changing_log_or_history() {
    for history_kind in 0..3 {
        let (cfg, anchor) = old_funded_anchor(&format!("old-unscanned-{history_kind}")).await;
        let root = cfg.node.log_dir.parent().unwrap();
        let mut connection = None;
        if history_kind != 0 {
            std::fs::create_dir_all(&cfg.history.dir).unwrap();
            let path = cfg.history.dir.join(crate::history::INDEX_FILE);
            if history_kind == 1 {
                std::fs::write(path, b"retain corrupt history for operator inspection").unwrap();
            } else {
                let db = rusqlite::Connection::open(path).unwrap();
                db.execute_batch("PRAGMA journal_mode=wal; CREATE TABLE diagnostic(value TEXT); INSERT INTO diagnostic VALUES('preserve funded anchor WAL');").unwrap();
                connection = Some(db);
            }
        }
        let before = files(root);
        let signer = AccountId(EnclaveKey::test_key().address());
        let (log, plan) =
            crate::node::tests::read_log_and_history_observe(&cfg, Some(signer), Some(anchor), |_| {}).unwrap();
        let (sink, mut work) = mpsc::channel(64);
        let seq_cfg = SequencerConfig {
            checkpoints: Some(sink),
            checkpoint_parent: anchor,
            genesis: crate::sequencer::genesis_commands(&cfg.genesis),
            ..sequencer_config(&cfg.node.log_dir, watch::channel(Manifest::default()).0)
        };
        let error = finish_preflight::<NoChain>(plan, None, Some((&log, &seq_cfg, signer))).await.err().unwrap();
        let message = format!("{error:#}");
        assert!(message.contains("nonempty unproven custody prefix"), "{message}");
        assert!(message.contains("preserve this prelaunch log"), "{message}");
        assert_eq!(files(root), before, "history kind {history_kind}: refusal must precede every mutation");
        assert!(work.try_recv().is_err(), "startup must not queue a successor signature");
        drop(connection);
    }
}

#[tokio::test]
async fn unscanned_outstanding_full_replay_refuses_startup() {
    reject_outstanding(false).await;
}

#[tokio::test]
async fn unscanned_outstanding_authenticated_snapshot_refuses_startup() {
    reject_outstanding(true).await;
}

/// Every restored successor can finalize after preflight, independently of its source.
async fn reject_outstanding(cached: bool) {
    let (cfg, successor) = old_funded_anchor(&format!("old-unscanned-queue-{cached}")).await;
    let key = EnclaveKey::test_key();
    let signer = AccountId(key.address());
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
    let last = log_entries(&cfg.node.log_dir).pop().unwrap();
    let LogEntry::Checkpoint(checkpoint) = last else { panic!("signed successor missing") };
    let anchor = checkpoint.parent.unwrap();
    assert!(anchor.seq < successor.seq);
    let mut log =
        Sequencer::read_log_for_recovery(&cfg.node.log_dir, Some(domain), Some(signer), Some(anchor), |_| {}).unwrap();
    if cached {
        let cache = crate::config::SnapshotSection {
            dir: cfg.node.log_dir.parent().unwrap().join("cache"),
            retain: std::num::NonZeroUsize::new(2).unwrap(),
            max_bytes: std::num::NonZeroUsize::new(32 * 1024 * 1024).unwrap(),
            trusted_signers: vec![signer],
        };
        log = log.test_authenticated_snapshot(&cache, &cfg.node.log_dir, anchor);
        assert_eq!(log.snapshot_sequence(), Some(successor.seq));
    }
    let root = cfg.node.log_dir.parent().unwrap();
    let before = files(root);
    let (sink, mut work) = mpsc::channel(64);
    let seq_cfg = SequencerConfig {
        checkpoints: Some(sink),
        checkpoint_parent: anchor,
        genesis: crate::sequencer::genesis_commands(&cfg.genesis),
        close_spread_ceilings: cfg.genesis.close_spread_ceilings.clone(),
        ..sequencer_config(&cfg.node.log_dir, watch::channel(Manifest::default()).0)
    };
    let plan = crate::history::HistoryPlan::open(&cfg.history).unwrap();
    let error = finish_preflight::<NoChain>(plan, None, Some((&log, &seq_cfg, signer))).await.err().unwrap();
    let message = format!("{error:#}");
    assert!(message.contains("nonempty unproven custody prefix"), "{message}");
    assert!(message.contains(&format!("seq {}", successor.seq)), "refuse B even though A is empty: {message}");
    assert_eq!(files(root), before, "cached={cached}: refusal precedes history repair or writer creation");
    assert!(work.try_recv().is_err());
}

/// Reproduces the earlier writer's valid signature after a real credit, without a frontier or successor.
async fn old_funded_anchor(name: &str) -> (NodeConfig, CheckpointParent) {
    let root = scratch_dir(name);
    let mut cfg = dev_config();
    cfg.node.log_dir = root.join("log");
    cfg.history.dir = root.join("history");
    cfg.genesis.house_capital = Fixed::ZERO;
    let key = Arc::new(EnclaveKey::test_key());
    let signer = AccountId(key.address());
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
    let (sink, _queued) = mpsc::channel(64);
    let seq_cfg = SequencerConfig {
        checkpoints: Some(sink),
        checkpoint_every: 1,
        genesis: crate::sequencer::genesis_commands(&cfg.genesis),
        close_spread_ceilings: cfg.genesis.close_spread_ceilings.clone(),
        ..sequencer_config(&cfg.node.log_dir, watch::channel(Manifest::default()).0)
    };
    let mut seq =
        Sequencer::recover(seq_cfg, key.clone(), broadcast::channel(128).0, mpsc::unbounded_channel().0).unwrap();
    seq.activate_protocol_at(ProtocolVersion::V23).unwrap();
    seq.genesis(&cfg.genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    let (tx, rx) = mpsc::channel(16);
    let task = tokio::spawn(seq.run(rx));
    let receipt = DepositReceipt {
        block: 1,
        block_hash: B256::repeat_byte(1),
        tx_hash: B256::repeat_byte(2),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 0,
        beneficiary: DepositBeneficiary::House,
        amount: Fixed::from_int(10),
    };
    let (reply, applied) = oneshot::channel();
    tx.send(Request::Submit {
        cmd: Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt })),
        reply: Some(reply),
    })
    .await
    .unwrap();
    assert!(!applied.await.unwrap().is_empty());
    drop(tx);
    task.await.unwrap().unwrap();
    let mut replayed = replay_path_with_trust(&cfg.node.log_dir, Some(domain), Some(signer)).unwrap();
    assert_eq!(replayed.engine.custody_checkpoint(), None);
    assert_eq!(replayed.engine.house_cash(), Fixed::from_int(10));
    let parent = log_entries(&cfg.node.log_dir)
        .into_iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(cp) => {
                Some(CheckpointParent { has_checkpoint: true, seq: cp.seq, root: cp.root.parse().unwrap() })
            }
            _ => None,
        })
        .unwrap();
    let seq = replayed.engine.last_seq().unwrap();
    let state_root = replayed.engine.state_root();
    let cp = Checkpoint {
        parent: Some(parent),
        seq,
        root: format!("0x{}", hex::encode(state_root)),
        signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, seq, &state_root, parent),
        signer: key.address_hex(),
        domain: Some(domain),
        deferred_withdrawals: true,
    };
    // The current writer prevents this publication; the low-level fixture preserves the older writer's behavior.
    let mut writer = SegmentedLog::open(&cfg.node.log_dir, |_| Ok(()))
        .unwrap()
        .into_writer(1024, watch::channel(Manifest::default()).0)
        .unwrap();
    writer.append(&LogEntry::Checkpoint(cp.clone())).unwrap();
    writer.sync().unwrap();
    drop(writer);
    let audit = replay_path_with_trust(&cfg.node.log_dir, Some(domain), Some(signer)).unwrap();
    assert_eq!(audit.engine.last_seq(), Some(cp.seq), "read-only root audit remains supported");
    assert!(matches!(log_entries(&cfg.node.log_dir).last(), Some(LogEntry::Checkpoint(last)) if last == &cp));
    (cfg, CheckpointParent { has_checkpoint: true, seq: cp.seq, root: cp.root.parse().unwrap() })
}
