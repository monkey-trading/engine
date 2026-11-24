//! Captures while the history index lags the log: a due capture publishes, paired with the history the
//! owner has indexed before its checkpoint, whether the index is held where it stands or the owner's input
//! runs a full queue ahead of it; newer caches keep replacing older ones, so what the caches pin moves on;
//! and a restart from such a cache resumes history from its head and indexes the commands between.

use std::sync::Arc;
use std::time::Instant;

use engine_core::command::Command;

use crate::clock::test_clock::TestClock;
use crate::clock::{Clock, SystemClock};
use crate::config::NodeConfig;
use crate::history::PageQuery;
use crate::log::CheckpointDomain;
use crate::node::paired_tests::anchor::seq_of;
use crate::node::paired_tests::cadence::{INTERVAL, caches, deposits, to_checkpoint};
use crate::node::paired_tests::harness::{HANG, Node, configuration};
use crate::node::paired_tests::publication::{collect_alerts, write_alert_lines};
use crate::snapshot;
use crate::test_support::{TRADER, logged_history};

/// Ticks sequenced while the owner is paused: more entries than its queue holds.
const BACKLOG: usize = 1_100;

/// With the index held where it stands, a due capture still publishes once the owner takes its checkpoint,
/// paired with the history indexed before it (`lagging_cache`). A restart from that cache resumes history
/// from its head and indexes the commands between, so the history served is the log's.
#[tokio::test]
async fn a_capture_publishes_with_the_index_held_behind_its_checkpoint() {
    let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
    let (cfg, checkpoint, _) = lagging_cache("paired-lagging-held", clock.clone()).await;
    let node = Node::start_at(cfg, clock).await.unwrap();
    assert_eq!(node.snapshot_seq, Some(checkpoint), "the restart resumes from the cache whose history trails it");
    let (cfg, reader) = node.stop_after_signing("after-held-restart").await;
    let query = PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
    assert_eq!(reader.page(query).await.unwrap().unwrap().events, logged_history(&cfg.node.log_dir, TRADER));
}

/// A capture due while the owner is paused, more entries queued behind the pause than its queue holds, is
/// paired at the end of the owner's next full turn with the history indexed by then, well before the
/// capture's checkpoint, instead of waiting for the backlog.
#[tokio::test]
async fn an_owner_behind_its_input_pairs_a_capture_without_waiting_for_the_backlog() {
    let mut cfg = configuration("paired-lagging-backlog");
    cfg.snapshots.as_mut().unwrap().interval = INTERVAL;
    let (collector, router) = collect_alerts();
    let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
    let node = Node::start_at(cfg, clock.clone()).await.unwrap();
    deposits(&node, "first", 3).await;
    let before = caches(&node);
    let release = node.sealing.pause().await.unwrap();
    for _ in 0..BACKLOG {
        node.submit(Command::Tick).await;
    }
    clock.advance(INTERVAL);
    let checkpoint = to_checkpoint(&node, "behind").await;
    let since = Instant::now();
    while !node.sealing.queue_full() {
        assert!(since.elapsed() < HANG, "the owner's queue never filled behind the pause");
        tokio::task::yield_now().await;
    }
    release.send(()).unwrap();
    node.history_caught_up().await;
    let taken = caches(&node);
    assert_eq!(taken.len(), before.len() + 1, "the capture behind the backlog publishes: {taken:?}");
    assert_eq!(seq_of(&taken[0]), checkpoint, "the cache is the checkpoint whose capture was due");
    let head = paired_head(&node.cfg, &taken[0]);
    let backlog = u64::try_from(BACKLOG).unwrap();
    assert!(
        head + backlog / 8 < checkpoint,
        "the cache pairs the history of the owner's first full turn, not the backlog's end: {head} for {checkpoint}"
    );
    let (_, reader) = node.stop().await;
    drop((reader, collector));
    let lines = write_alert_lines(router);
    assert!(lines.iter().all(|line| line.severity.is_none()), "no capture failed: {lines:#?}");
}

/// While the index lags, each due capture still publishes, so newer caches replace older ones and the
/// oldest version anything pins, below which collection retires versions, moves on from the first cache.
#[tokio::test]
async fn newer_caches_replace_older_ones_while_the_index_lags() {
    let mut cfg = configuration("paired-lagging-pins");
    let section = cfg.snapshots.as_mut().unwrap();
    (section.interval, section.retain) = (INTERVAL, 2.try_into().unwrap());
    let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
    let node = Node::start_at(cfg, clock.clone()).await.unwrap();
    deposits(&node, "first", 3).await;
    let first = node.history_caught_up().await.pinned_floor;
    let mut heads = Vec::new();
    for round in 0..3 {
        clock.advance(INTERVAL);
        node.sealing.hold_index(true);
        let checkpoint = to_checkpoint(&node, &format!("lagging-{round}")).await;
        node.history_caught_up().await;
        let newest = caches(&node).remove(0);
        assert_eq!(seq_of(&newest), checkpoint, "round {round}: the capture publishes while the index lags");
        heads.push(paired_head(&node.cfg, &newest));
        // The index catches up, then lags again in the next round.
        node.sealing.hold_index(false);
        deposits(&node, &format!("caught-up-{round}"), 1).await;
    }
    assert!(heads.windows(2).all(|pair| pair[0] < pair[1]), "each newer cache pairs later history: {heads:?}");
    assert_eq!(caches(&node).len(), 2, "the newer caches replaced the older ones");
    let floor = node.history_caught_up().await.pinned_floor;
    assert!(floor > first, "the oldest pinned version moves on from the first cache's {first}: {floor}");
    node.stop().await;
}

/// A node's log and caches, stopped with its newest cache published while the index was held behind its
/// checkpoint, at `clock`: the config, that checkpoint's seq, and the caches before it, newest first, each
/// paired at its checkpoint. No signed history head reaches the checkpoint, the index held to the stop.
pub(super) async fn lagging_cache(label: &str, clock: Arc<TestClock>) -> (NodeConfig, u64, Vec<std::path::PathBuf>) {
    let mut cfg = configuration(label);
    cfg.snapshots.as_mut().unwrap().interval = INTERVAL;
    let node = Node::start_at(cfg, clock.clone()).await.unwrap();
    deposits(&node, "first", 3).await;
    let before = caches(&node);
    clock.advance(INTERVAL);
    node.sealing.hold_index(true);
    let checkpoint = to_checkpoint(&node, "held").await;
    node.history_caught_up().await;
    let taken = caches(&node);
    assert_eq!(taken.len(), before.len() + 1, "the capture publishes with the index held: {taken:?}");
    assert_eq!(seq_of(&taken[0]), checkpoint, "the cache is the checkpoint whose capture was due");
    let head = paired_head(&node.cfg, &taken[0]);
    assert!(head < checkpoint, "the cache pairs the history indexed before its checkpoint, at {head}");
    let (cfg, reader) = node.stop().await;
    drop(reader);
    (cfg, checkpoint, before)
}

/// The seq of the history head the cache at `path` pairs with.
pub(super) fn paired_head(cfg: &NodeConfig, path: &std::path::Path) -> u64 {
    let section = cfg.snapshots.as_ref().unwrap();
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() };
    let (payload, _) = snapshot::read(section, path, domain.chain_id, domain.vault).unwrap();
    let pair = payload.pair.expect("a paired cache");
    let statement = snapshot::head::verify_certificate(section, &pair.history, domain).unwrap();
    statement.head.last_seq().expect("a complete head")
}
