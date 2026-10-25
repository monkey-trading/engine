//! The on-chain checkpoint preflight after paired recovery. The finalized checkpoint the chain
//! reports can be a cache's base, a checkpoint in its outstanding work, or one in the log suffix;
//! recovery must hand it to the preflight in each case, and a log ending before it is refused.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use engine_core::Fixed;
use engine_core::types::AccountId;
use tokio::sync::{mpsc, watch};

use crate::checkpoints::{CheckpointChain, Poster, VaultState, View};
use crate::config::NodeConfig;
use crate::log::{Checkpoint, CheckpointDomain, CheckpointParent, LogEntry, Manifest};
use crate::node::paired_tests::harness::{Node, configuration};
use crate::node::{preflight, read_log_and_history_observe};
use crate::sequencer::Request;
use crate::snapshot;
use crate::tee::EnclaveKey;
use crate::test_support::{deposit, sequencer_config};

struct Finalized(VaultState);
impl CheckpointChain for Finalized {
    async fn state(&self, _: View) -> anyhow::Result<VaultState> {
        Ok(self.0.clone())
    }
    async fn post(&self, _: &Checkpoint) -> anyhow::Result<()> {
        unreachable!("startup posts nothing")
    }
}

#[tokio::test]
async fn paired_recovery_hands_the_finalized_checkpoint_to_the_preflight_wherever_it_lies() {
    let (cfg, checkpoints, base) = gated_log().await;
    let caches = snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap();
    let newest = seq_of(&caches[0]);
    let in_work = checkpoints
        .iter()
        .find(|checkpoint| checkpoint.seq > base.seq && checkpoint.seq < newest)
        .expect("the newest cache carries outstanding work after its base");
    let last = checkpoints.last().unwrap();
    for (case, finalized) in [("base", &base), ("work", in_work), ("suffix", last)] {
        let copy = copy_of(&cfg, case);
        if case == "suffix" {
            // Recovery then uses an earlier cache, so the finalized checkpoint lies in the suffix.
            for path in snapshot::candidates_readonly(copy.snapshots.as_ref().unwrap()).unwrap() {
                if seq_of(&path) >= finalized.seq {
                    fs::write(path, b"{").unwrap();
                }
            }
        }
        let cache = recover(&copy, finalized).await.unwrap_or_else(|error| panic!("{case}: {error:#}"));
        let cache = cache.unwrap_or_else(|| panic!("{case}: recovery used a paired cache"));
        assert!(cache >= base.seq, "{case}: the cache is gated on the finalized base");
        match case {
            "suffix" => assert!(cache < finalized.seq, "the finalized checkpoint follows the cache"),
            _ => assert_eq!(cache, newest, "{case}: the newest cache recovers"),
        }
    }

    let copy = copy_of(&cfg, "truncated");
    let beyond = Checkpoint { seq: last.seq + 4, root: format!("0x{}", "ab".repeat(32)), ..last.clone() };
    let root = copy.node.log_dir.parent().unwrap().to_owned();
    let before = files(&root);
    assert!(recover(&copy, &beyond).await.is_err(), "a log that ends before the finalized checkpoint is refused");
    assert_eq!(files(&root), before, "the refusal changed no byte");
}

/// A gated paired node's log: the chain finalizes its first checkpoint, every later one stays
/// outstanding work, and history catches up after each command so every checkpoint is captured.
/// Returns the stopped node's config, every checkpoint in log order, and the finalized one.
async fn gated_log() -> (NodeConfig, Vec<Checkpoint>, Checkpoint) {
    let mut cfg = configuration("paired-anchor");
    cfg.node.checkpoint_every = 2;
    let (work, mut sent) = mpsc::channel(1024);
    let node = Node::start_gated(cfg, CheckpointParent::default(), work).await.unwrap();
    let mut checkpoints = Vec::new();
    let mut n = 0;
    while checkpoints.is_empty() {
        node.submit(deposit(1, &format!("anchor-{n}"))).await;
        n += 1;
        while let Ok(work) = sent.try_recv() {
            checkpoints.push(work.checkpoint);
        }
    }
    let base = checkpoints[0].clone();
    node.tx.send(Request::CheckpointStatus { posted: Some(base.seq), healthy: true, lifecycle: None }).await.unwrap();
    for _ in 0..12 {
        node.submit(deposit(1, &format!("anchor-{n}"))).await;
        n += 1;
        node.history_caught_up().await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    while let Ok(work) = sent.try_recv() {
        checkpoints.push(work.checkpoint);
    }
    (cfg, checkpoints, base)
}

/// Runs the node's read-only recovery and its preflight against a chain whose finalized checkpoint
/// is `finalized`, the way startup does; returns the cache recovery used.
async fn recover(cfg: &NodeConfig, finalized: &Checkpoint) -> anyhow::Result<Option<u64>> {
    let key = EnclaveKey::test_key();
    let parent = CheckpointParent { has_checkpoint: true, seq: finalized.seq, root: finalized.root.parse()? };
    let mut anchor = None;
    let (log, _plan) = read_log_and_history_observe(cfg, Some(AccountId(key.address())), Some(parent), |entry| {
        if let LogEntry::Checkpoint(checkpoint) = entry
            && checkpoint.seq == parent.seq
        {
            anchor = Some(checkpoint.clone());
        }
    })?;
    let poster = Poster {
        registry: None,
        chain: Finalized(VaultState {
            height: 0,
            signer: AccountId(key.address()),
            checkpoint: Some((finalized.seq, finalized.root.clone())),
            balance: Fixed::ZERO,
            deposits: 0,
            deposited: Fixed::ZERO,
        }),
        domain: CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? },
        signer: AccountId(key.address()),
        retry: Duration::from_millis(1),
        pending_deposits_critical_after: Duration::from_secs(1),
    };
    let mut seq_cfg = sequencer_config(&cfg.node.log_dir, watch::channel(Manifest::default()).0);
    seq_cfg.checkpoint_parent = parent;
    seq_cfg.checkpoints = Some(mpsc::channel(1).0);
    preflight::authorize(Some((&poster, anchor.as_ref())), Some((&log, &seq_cfg, AccountId(key.address())))).await?;
    Ok(log.snapshot_sequence())
}

/// A fresh node config holding copies of `cfg`'s log and cache directories.
fn copy_of(cfg: &NodeConfig, case: &str) -> NodeConfig {
    let mut copy = configuration(&format!("paired-anchor-{case}"));
    copy.node.checkpoint_every = cfg.node.checkpoint_every;
    let dirs = [
        (&cfg.node.log_dir, &copy.node.log_dir),
        (&cfg.snapshots.as_ref().unwrap().dir, &copy.snapshots.as_ref().unwrap().dir),
    ];
    for (from, to) in dirs {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
    copy
}

/// Every file under `root` and its bytes.
fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(files(&path));
        } else {
            result.insert(path.clone(), fs::read(path).unwrap());
        }
    }
    result
}

/// The checkpoint seq a cache file is named for.
fn seq_of(path: &Path) -> u64 {
    path.file_name().unwrap().to_str().unwrap()[..20].parse().unwrap()
}
