//! The restart dev scenario: a node with snapshots restarts from its newest paired cache as the log
//! and history grow tenfold, then skips a corrupt newest cache. Each restarted node signs a
//! checkpoint over the state it recovered, which a full audit replay must verify.

use std::fs;
use std::time::Instant;

use tokio::time::timeout;

use crate::config::NodeConfig;
use crate::history::{Page, PageQuery, transient};
use crate::log::{LogEntry, Offset};
use crate::node::paired_tests::harness::{HANG, Node, configuration};
use crate::snapshot;
use crate::test_support::{TRADER, capture_alerts, deposit, log_entries, logged_history};
use crate::trusted_time::tests::logs::Logs;

const CHECKPOINT_EVERY: u64 = 100;

/// Each restart replays at most one checkpoint interval of commands and serves complete history at
/// once; it prints its timings rather than gating on them. A corrupt newest cache raises a
/// `snapshot_rejected` Warning and recovery uses an earlier cache with the same history.
#[tokio::test]
async fn dev_scenario_restart_stays_bounded_as_the_log_grows_and_skips_a_corrupt_cache() {
    let mut cfg = configuration("dev-scenario-paired-restart");
    cfg.node.segment_commands = CHECKPOINT_EVERY;
    cfg.node.checkpoint_every = CHECKPOINT_EVERY;
    let mut submitted = 0;
    for target in [200, 2_000] {
        let node = Node::start(cfg).await.unwrap();
        while submitted < target {
            node.submit(deposit(1, &format!("restart-{submitted}"))).await;
            submitted += 1;
            if submitted % CHECKPOINT_EVERY == 0 {
                // Thousands of entries queued ahead of the owner make one turn index a whole queue
                // batch, and one turn is the step history_caught_up waits on; staying within one
                // checkpoint interval keeps every turn short, however long the catch-up takes.
                node.history_caught_up().await;
            }
        }
        // A burst can outrun history; the scenario restarts a node that has caught up.
        node.history_caught_up().await;
        let (next, reader) = node.stop().await;
        drop(reader);
        cfg = next;
        let started = Instant::now();
        let node = Node::start(cfg).await.unwrap();
        let restart = started.elapsed();
        let cache = node.snapshot_seq.expect("restart uses a paired cache");
        let suffix = last_seq(&node.cfg) - cache;
        assert!(suffix <= CHECKPOINT_EVERY, "replayed {suffix} commands after the cache");
        assert_complete_history(&node).await;
        eprintln!(
            "restart scenario: deposits={target} log_commands={} restart_ms={} suffix_commands={suffix}",
            last_seq(&node.cfg) + 1,
            restart.as_millis()
        );
        let (next, reader) = node.stop_after_signing(&format!("after-restart-{target}")).await;
        drop(reader);
        cfg = next;
    }

    let caches = snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap();
    assert!(caches.len() > 1, "an earlier cache exists to fall back to");
    let newest = seq_of(&caches[0]);
    fs::write(&caches[0], b"{\"payload\":").unwrap();
    let (_guard, mut alerts) = capture_alerts();
    let node = Node::start(cfg).await.unwrap();
    let alert = alerts.try_recv().expect("the corrupt cache raises an alert");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("snapshot_rejected", "WARN"));
    let fallback = node.snapshot_seq.expect("an earlier paired cache recovers the node");
    assert!(fallback < newest, "recovery skipped the corrupt newest cache");
    assert_complete_history(&node).await;
    node.stop_after_signing("after-fallback").await;
}

/// With no signer pinned, a restart skips its caches with one plain line rather than a
/// `snapshot_rejected` Warning per cache, and recovers complete history by full replay.
#[tokio::test]
async fn restart_without_signer_pins_skips_every_cache_with_one_line() {
    let mut cfg = configuration("paired-restart-without-pins");
    cfg.snapshots.as_mut().unwrap().trusted_signers.clear();
    let node = Node::start(cfg).await.unwrap();
    // History catches up after each command, so no capture finds the previous one in flight.
    for n in 0..9 {
        node.submit(deposit(1, &format!("unpinned-{n}"))).await;
        node.history_caught_up().await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let caches = snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap();
    assert!(caches.len() > 1, "the unpinned node wrote several caches");
    let (logs, _guard) = Logs::capture();
    let node = Node::start(cfg).await.unwrap();
    assert_eq!(logs.warnings("snapshot_rejected"), 0, "no cache raises a Warning");
    assert_eq!(logs.count("no snapshot signer is pinned"), 1, "one line names full replay");
    assert_eq!(node.snapshot_seq, None, "recovery replays the whole log");
    assert_complete_history(&node).await;
    node.stop().await;
}

/// Every retained event for the trader, paged from the restarted node before it takes any command.
async fn assert_complete_history(node: &Node) {
    let expected = logged_history(&node.cfg.node.log_dir, TRADER);
    let mut served = Vec::new();
    let mut before = None;
    loop {
        let page = page(node, before).await;
        served.extend(page.events);
        before = page.next;
        if before.is_none() {
            break;
        }
    }
    assert_eq!(served, expected, "every retained event survives the restart exactly once");
}

/// One page, retrying while the index writer's commit outlasts the reader's busy timeout; a page
/// read fails only when no attempt answers for `HANG`. Other errors mean a damaged index, which
/// repair must not let this test pass off.
async fn page(node: &Node, before: Option<Offset>) -> Page {
    let started = Instant::now();
    loop {
        let query = PageQuery { account: TRADER, kind: None, before, after: None, limit: 200 };
        match timeout(HANG, node.reader.page(query)).await {
            Ok(Ok(Some(page))) => return page,
            Ok(Ok(None)) => panic!("history serves at once after a paired restart"),
            Ok(Err(error)) if transient(&error) => {
                assert!(started.elapsed() < HANG, "history stops answering pages: {error:#}");
                tokio::task::yield_now().await;
            }
            Ok(Err(error)) => panic!("history page fails: {error:#}"),
            Err(_) => assert!(started.elapsed() < HANG, "a history page stays pending past {HANG:?}"),
        }
    }
}

/// The seq of the last command in the log.
fn last_seq(cfg: &NodeConfig) -> u64 {
    log_entries(&cfg.node.log_dir)
        .iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Command(command) => Some(command.sequenced.seq),
            _ => None,
        })
        .expect("the log holds a command")
}

/// The checkpoint seq a cache file is named for.
fn seq_of(path: &std::path::Path) -> u64 {
    path.file_name().unwrap().to_str().unwrap()[..20].parse().unwrap()
}
