//! The newest caches and heads around the end of the log: a final checkpoint whose capture was
//! skipped, a log tail lost after history signed a head there, and no retained head that resumes.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use crate::history::PageQuery;
use crate::log::CheckpointParent;
use crate::node::paired_tests::harness::{HANG, Node, configuration};
use crate::sequencer::Request;
use crate::snapshot;
use crate::test_support::{TRADER, deposit, logged_history};

/// A batch spanning two checkpoints captures only the first, since the second finds it in flight.
/// When the batch ends on that second checkpoint, shutdown still caches it.
#[tokio::test]
async fn shutdown_caches_a_final_checkpoint_whose_capture_was_skipped() {
    let node = Node::start(configuration("paired-skipped-final-capture")).await.unwrap();
    node.history_caught_up().await;
    let final_checkpoint = burst_to_second_checkpoint(&node).await;
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let newest = snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap().remove(0);
    assert_eq!(seq_of(&newest), final_checkpoint, "the final checkpoint is cached");
}

/// Sequences one batch spanning two checkpoints and ending on the second, whose capture finds the
/// first in flight; returns the second checkpoint's seq.
async fn burst_to_second_checkpoint(node: &Node) -> u64 {
    let every = node.cfg.node.checkpoint_every;
    let last = node.manifest.borrow().flushed.expect("genesis is logged").seq;
    let first = (last + 1..).find(|seq| (seq + 1) % every == 0).unwrap();
    let final_checkpoint = first + every;
    // Queued before the sequencer runs, so they form one batch.
    let mut replies = Vec::new();
    for n in last + 1..=final_checkpoint {
        let (reply, done) = oneshot::channel();
        node.tx.send(Request::Submit { cmd: deposit(1, &format!("burst-{n}")), reply: Some(reply) }).await.unwrap();
        replies.push(done);
    }
    for done in replies {
        timeout(HANG, done).await.unwrap().unwrap();
    }
    final_checkpoint
}

/// The log ends before a signed history head when storage loses synced writes or the host truncates
/// it. Restart still uses the cache, alerts, and resumes history from the cache's own head.
#[tokio::test]
async fn restart_after_the_log_lost_its_tail_resumes_history_from_the_cache_head() {
    let mut cfg = configuration("paired-lost-tail");
    cfg.node.segment_commands = 1_000;
    cfg.snapshots.as_mut().unwrap().retain = 16.try_into().unwrap();
    let node = Node::start(cfg).await.unwrap();
    for n in 0..6 {
        node.submit(deposit(2, &format!("kept-{n}"))).await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let dir = cfg.snapshots.as_ref().unwrap().dir.clone();
    let segment = cfg.node.log_dir.join("00000000000000000000.jsonl");
    let synced = fs::read(&segment).unwrap();
    let kept: BTreeSet<PathBuf> =
        snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap().into_iter().collect();
    let catalog_heads: Vec<(PathBuf, Vec<u8>)> =
        catalog_head_files(&dir).into_iter().map(|path| (path.clone(), fs::read(path).unwrap())).collect();

    let node = Node::start(cfg).await.unwrap();
    for n in 0..6 {
        node.submit(deposit(2, &format!("lost-{n}"))).await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    // The loss keeps the log, caches, and catalog heads from before the second run.
    fs::write(&segment, &synced).unwrap();
    for path in snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap() {
        if !kept.contains(&path) {
            fs::remove_file(path).unwrap();
        }
    }
    for path in catalog_head_files(&dir) {
        fs::remove_file(path).unwrap();
    }
    for (path, bytes) in &catalog_heads {
        fs::write(path, bytes).unwrap();
    }

    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let node = Node::start(cfg).await.unwrap();
    assert!(node.snapshot_seq.is_some(), "a head past the log must not force full replay");
    let lost = std::iter::from_fn(|| alerts.try_recv().ok())
        .find(|alert| alert["alert"] == "log_tail_lost")
        .expect("the lost tail raises an alert");
    assert_eq!(lost["level"], "ERROR");
    let (cfg, reader) = node.stop_after_signing("after-lost-tail").await;
    let query = PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
    assert_eq!(reader.page(query).await.unwrap().unwrap().events, logged_history(&cfg.node.log_dir, TRADER));
}

/// With no retained history head that resumes, the cache's own head carries history.
#[tokio::test]
async fn restart_without_a_resumable_history_head_uses_the_cache_head() {
    let node = Node::start(configuration("paired-no-history-head")).await.unwrap();
    for n in 0..7 {
        node.submit(deposit(2, &format!("headless-{n}"))).await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    for path in snapshot::head::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap() {
        fs::remove_file(path).unwrap();
    }
    let node = Node::start(cfg).await.unwrap();
    assert!(node.snapshot_seq.is_some(), "missing history heads must not force full replay");
    let (cfg, reader) = node.stop_after_signing("after-headless-restart").await;
    let query = PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
    assert_eq!(reader.page(query).await.unwrap().unwrap().events, logged_history(&cfg.node.log_dir, TRADER));
}

/// The chain can finalize a checkpoint after its capture was skipped and before shutdown; the
/// shutdown capture still uses the recovery facts the checkpoint was written with.
#[tokio::test]
async fn shutdown_caches_a_skipped_checkpoint_the_chain_finalized_meanwhile() {
    let (work, mut sent) = mpsc::channel(1024);
    let node =
        Node::start_gated(configuration("paired-skipped-finalized"), CheckpointParent::default(), work).await.unwrap();
    let mut n = 0;
    let base = loop {
        node.submit(deposit(1, &format!("gated-{n}"))).await;
        n += 1;
        if let Ok(work) = sent.try_recv() {
            break work.checkpoint;
        }
    };
    node.tx.send(Request::CheckpointStatus { posted: Some(base.seq), healthy: true, lifecycle: None }).await.unwrap();
    node.history_caught_up().await;
    let final_checkpoint = burst_to_second_checkpoint(&node).await;
    node.tx
        .send(Request::CheckpointStatus { posted: Some(final_checkpoint), healthy: true, lifecycle: None })
        .await
        .unwrap();
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let newest = snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap().remove(0);
    assert_eq!(seq_of(&newest), final_checkpoint, "the final checkpoint is cached");
}

/// The signed catalog heads in a cache directory.
fn catalog_head_files(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("catalog-head-") && name.ends_with(".json"))
        })
        .collect()
}

/// The checkpoint seq a cache file is named for.
fn seq_of(path: &Path) -> u64 {
    path.file_name().unwrap().to_str().unwrap()[..20].parse().unwrap()
}
