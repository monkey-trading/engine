//! The newest caches and heads around the end of the log: a final checkpoint whose capture was
//! skipped, a log tail lost after history or the catalog signed heads there, and no retained head
//! that resumes.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use crate::checkpoints::Health;
use crate::history::PageQuery;
use crate::log::CheckpointParent;
use crate::node::paired_tests::harness::{HANG, Node, configuration};
use crate::sequencer::Request;
use crate::snapshot;
use crate::test_support::{TRADER, capture_alerts, deposit, logged_history};

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

/// Shutdown waits at most SHUTDOWN_CAPTURE_WAIT for a checkpoint capture still in flight; when the
/// capture cannot complete in that bound it warns and the skipped final checkpoint stays uncached.
/// The test holds the capture permit itself for the whole stop, so the capture stays in flight
/// whatever the history owner's speed and no scheduling order can change the outcome.
#[tokio::test]
async fn shutdown_warns_and_skips_the_final_checkpoint_when_its_capture_stays_in_flight() {
    let node = Node::start(configuration("paired-shutdown-capture-times-out")).await.unwrap();
    node.history_caught_up().await;
    let final_checkpoint = burst_to_second_checkpoint(&node).await;
    let _in_flight = node.sealing.try_permit().expect("history completed the first capture");
    let (_guard, mut alerts) = capture_alerts();
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let alert = alerts.try_recv().expect("a capture still in flight when the wait ends warns");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("snapshot_write_failed", "WARN"));
    let newest = snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap().remove(0);
    assert_ne!(seq_of(&newest), final_checkpoint, "the final checkpoint whose capture stayed in flight is not cached");
}

/// Shutdown waits for a capture still in flight; once the capture completes the wait resolves and
/// the skipped final checkpoint is cached. The test holds the capture permit itself and releases
/// it only once shutdown waits, so the wait's success is covered whatever the history owner's
/// speed.
#[tokio::test]
async fn shutdown_waits_for_a_capture_in_flight_and_caches_the_final_checkpoint() {
    let node = Node::start(configuration("paired-shutdown-capture-in-flight")).await.unwrap();
    node.history_caught_up().await;
    let final_checkpoint = burst_to_second_checkpoint(&node).await;
    let sealing = node.sealing.clone();
    let in_flight = sealing.try_permit().expect("history completed the first capture");
    let (_guard, mut alerts) = capture_alerts();
    let stopped = tokio::spawn(async move { node.stop().await });
    let started = Instant::now();
    while sealing.waiting.load(Ordering::Acquire) == 0 {
        assert!(!stopped.is_finished(), "shutdown completed without waiting for the in-flight capture");
        assert!(started.elapsed() < HANG, "shutdown never waits for the in-flight capture");
        tokio::task::yield_now().await;
    }
    drop(in_flight);
    let (cfg, reader) = stopped.await.unwrap();
    drop(reader);
    assert!(alerts.try_recv().is_err(), "no warning: the wait ends once the capture completes");
    let newest = snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap().remove(0);
    assert_eq!(seq_of(&newest), final_checkpoint, "the final checkpoint is cached once the wait resolves");
}

/// Sequences one batch spanning two checkpoints and ending on the second, whose capture finds the
/// first in flight, then waits for history to catch up, so that capture is done before shutdown;
/// returns the second checkpoint's seq.
async fn burst_to_second_checkpoint(node: &Node) -> u64 {
    let every = node.cfg.node.checkpoint_every;
    let last = node.manifest.borrow().flushed.expect("genesis is logged").seq;
    let first = (last + 1..).find(|seq| (seq + 1) % every == 0).unwrap();
    let final_checkpoint = first + every;
    // A fresh time sample and the commands are queued before the sequencer runs, so they form one
    // batch.
    node.refresh_time().await;
    let mut replies = Vec::new();
    for n in last + 1..=final_checkpoint {
        let (reply, done) = oneshot::channel();
        node.tx.send(Request::Submit { cmd: deposit(1, &format!("burst-{n}")), reply: Some(reply) }).await.unwrap();
        replies.push(done);
    }
    for done in replies {
        timeout(HANG, done).await.unwrap().unwrap();
    }
    node.history_caught_up().await;
    final_checkpoint
}

/// The host truncates the active segment between two captured checkpoints; every cache and signed
/// head stays. History and the catalog signed only synced entries past the log's end, so restart
/// uses the older cache, alerts, and resumes both from it: the checkpoint sequenced again where the
/// lost one stood is cached, and so is the segment it completes.
#[tokio::test]
async fn restart_after_the_log_lost_its_active_tail_resumes_history_and_the_catalog_from_the_cache() {
    let mut cfg = configuration("paired-lost-tail");
    cfg.node.segment_commands = 40;
    cfg.snapshots.as_mut().unwrap().retain = 16.try_into().unwrap();
    let node = Node::start(cfg).await.unwrap();
    for n in 0..6 {
        node.submit(deposit(2, &format!("kept-{n}"))).await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let synced = files(&cfg.node.log_dir);

    let node = Node::start(cfg).await.unwrap();
    for n in 0..6 {
        node.submit(deposit(2, &format!("lost-{n}"))).await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let manifest = cfg.node.log_dir.join(crate::log::MANIFEST);
    assert_eq!(fs::read(&manifest).ok(), synced.get(&manifest).cloned(), "the lost entries stay in one segment");
    restore(&cfg.node.log_dir, &synced);
    resume_past_lost_tail(cfg).await;
}

/// The host truncates the log back across a completed segment the catalog tip certified, and
/// storage also lost the history heads signed since: the catalog tip alone shows the loss.
#[tokio::test]
async fn restart_after_the_log_lost_a_completed_segment_resumes_from_the_cache() {
    let mut cfg = configuration("paired-lost-segment");
    cfg.node.segment_commands = 40;
    cfg.snapshots.as_mut().unwrap().retain = 16.try_into().unwrap();
    let node = Node::start(cfg).await.unwrap();
    for n in 0..6 {
        node.submit(deposit(2, &format!("kept-{n}"))).await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let caches = cfg.snapshots.as_ref().unwrap().dir.clone();
    let synced = files(&cfg.node.log_dir);
    let heads: BTreeMap<PathBuf, Vec<u8>> = files(&caches)
        .into_iter()
        .filter(|(path, _)| path.file_name().unwrap().to_str().unwrap().starts_with("history-head-"))
        .collect();

    let node = Node::start(cfg).await.unwrap();
    let completed = completed_up_to(&node);
    let mut n = 0;
    while completed_up_to(&node) == completed {
        node.submit(deposit(2, &format!("lost-{n}"))).await;
        n += 1;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let tip: serde_json::Value = serde_json::from_slice(
        files(&caches)
            .iter()
            .filter(|(path, _)| path.file_name().unwrap().to_str().unwrap().starts_with("catalog-head-"))
            .map(|(_, bytes)| bytes)
            .next_back()
            .unwrap(),
    )
    .unwrap();
    assert!(tip["body"]["catalog"]["next_seq"].as_u64().unwrap() > completed, "the tip certifies the lost segment");
    restore(&cfg.node.log_dir, &synced);
    for path in snapshot::head::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap() {
        fs::remove_file(path).unwrap();
    }
    for (path, bytes) in &heads {
        fs::write(path, bytes).unwrap();
    }
    resume_past_lost_tail(cfg).await;
}

/// Restarts on a log that lost its tail: recovery uses an older cache and alerts. The node then
/// sequences until the segment holding the newest lost checkpoint completes, and must cache past it
/// and restart from that cache.
async fn resume_past_lost_tail(cfg: crate::config::NodeConfig) {
    let lost = seq_of(&snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap()[0]);
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let node = Node::start(cfg).await.unwrap();
    assert!(node.snapshot_seq.is_some_and(|seq| seq < lost), "a signed head past the log must not force full replay");
    let alert = std::iter::from_fn(|| alerts.try_recv().ok())
        .find(|alert| alert["alert"] == "log_tail_lost")
        .expect("the lost tail raises an alert");
    assert_eq!(alert["level"], "ERROR");
    // Before the node writes anything, no signed catalog head past the rewound catalog remains to
    // name a discarded version, so a restart still uses a cache.
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let node = Node::start(cfg).await.unwrap();
    assert!(node.snapshot_seq.is_some(), "a restart right after the rewind uses a cache");
    let mut n = 0;
    while completed_up_to(&node) <= lost {
        node.submit(deposit(1, &format!("again-{n}"))).await;
        n += 1;
    }
    let completed = completed_up_to(&node);
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let newest = seq_of(&snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap()[0]);
    assert!(newest >= completed, "the node caches past the segment that held the lost checkpoint");

    let node = Node::start(cfg).await.unwrap();
    assert_eq!(node.snapshot_seq, Some(newest), "restart uses the newest cache");
    let (cfg, reader) = node.stop_after_signing("after-lost-tail").await;
    let query = PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
    assert_eq!(reader.page(query).await.unwrap().unwrap().events, logged_history(&cfg.node.log_dir, TRADER));
}

/// The first seq after the completed segments the node's catalog holds.
fn completed_up_to(node: &Node) -> u64 {
    node.manifest.borrow().catalog.as_ref().expect("an authenticated catalog").next_seq()
}

/// Every file in `dir` with its bytes.
fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .map(|path| (path.clone(), fs::read(path).unwrap()))
        .collect()
}

/// Puts `dir` back to `kept`, as storage that lost synced writes or a host truncating it would.
fn restore(dir: &Path, kept: &BTreeMap<PathBuf, Vec<u8>>) {
    for path in files(dir).into_keys().filter(|path| !kept.contains_key(path)) {
        fs::remove_file(path).unwrap();
    }
    for (path, bytes) in kept {
        fs::write(path, bytes).unwrap();
    }
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

/// With no catalog head that verifies, the cache's own boundary carries the catalog: restart keeps
/// the generation, discards its later versions, and caches again.
#[tokio::test]
async fn restart_without_a_verified_catalog_head_resumes_the_catalog_from_the_cache() {
    let node = Node::start(configuration("paired-no-catalog-head")).await.unwrap();
    let catalog = node.sealing.catalog_store;
    for n in 0..7 {
        node.submit(deposit(2, &format!("tipless-{n}"))).await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let caches = cfg.snapshots.as_ref().unwrap().dir.clone();
    for path in files(&caches).into_keys() {
        if path.file_name().unwrap().to_str().unwrap().starts_with("catalog-head-") {
            fs::remove_file(path).unwrap();
        }
    }
    let node = Node::start(cfg).await.unwrap();
    assert!(node.snapshot_seq.is_some(), "missing catalog heads must not force full replay");
    assert_eq!(node.sealing.catalog_store, catalog, "the catalog generation resumed instead of being rebuilt");
    let (cfg, reader) = node.stop_after_signing("after-tipless-restart").await;
    let query = PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
    assert_eq!(reader.page(query).await.unwrap().unwrap().events, logged_history(&cfg.node.log_dir, TRADER));
    drop(reader);
    let newest = seq_of(&snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap()[0]);
    let node = Node::start(cfg).await.unwrap();
    assert_eq!(node.snapshot_seq, Some(newest), "the resumed catalog caches again");
    node.stop().await;
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
    node.tx
        .send(Request::CheckpointStatus { posted: Some(base.seq), health: Health::Healthy, lifecycle: None })
        .await
        .unwrap();
    node.history_caught_up().await;
    let final_checkpoint = burst_to_second_checkpoint(&node).await;
    node.tx
        .send(Request::CheckpointStatus { posted: Some(final_checkpoint), health: Health::Healthy, lifecycle: None })
        .await
        .unwrap();
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let newest = snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap().remove(0);
    assert_eq!(seq_of(&newest), final_checkpoint, "the final checkpoint is cached");
}

/// The checkpoint seq a cache file is named for.
fn seq_of(path: &Path) -> u64 {
    path.file_name().unwrap().to_str().unwrap()[..20].parse().unwrap()
}
