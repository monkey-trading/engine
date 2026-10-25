//! Real producer/actor failures must keep publication attempts and protected versions bounded, and
//! page as one alert episode.

use std::collections::BTreeSet;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use engine_core::command::Command;
use tokio::time::timeout;
use tracing::subscriber::set_default;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::registry;

use crate::alerts::{self, Routes, is_alert_callsite};
use crate::metrics::Metrics;
use crate::node::paired_tests::harness::{Node, configuration};
use crate::snapshot::head;
use crate::test_support::{deposit, keep_callsites_interested};

#[tokio::test]
async fn repeated_history_publication_failures_do_not_accumulate_unpublished_catalog_pins() {
    let node = Node::start(configuration("paired-failed-publication-pins")).await.unwrap();
    for n in 0..6 {
        node.submit(deposit(1, &format!("durable-{n}"))).await;
    }
    let before = at_frontier(&node).await;
    assert!(before.history_pins > 0, "actual durable history roots are protected");
    let caches = node.cfg.snapshots.as_ref().unwrap();
    // This blocks only H staging cleanup; signed T publication and authoritative logging continue.
    // Inventory correctly ignores a staging name, so the failure is in the real publication path.
    let blocked_stage = caches.dir.join("history-head-99999999999999999999.json.tmp");
    fs::create_dir(&blocked_stage).unwrap();
    for n in 0..30 {
        node.submit(deposit(1, &format!("failed-attempt-{n}"))).await;
    }
    let failed = at_frontier(&node).await;
    assert!(failed.last_entry > before.last_entry, "history still consumes the authoritative command/checkpoint feed");
    assert!(failed.catalog_pins <= before.catalog_pins, "unpublished failed C attempts cannot accumulate root leases");
    assert!(failed.history_pins <= before.history_pins + 1, "only one uncertain optional publication may add a pin");
    fs::remove_dir(blocked_stage).unwrap();
    for _ in 0..6 {
        node.submit(Command::Tick).await;
    }
    let repaired = at_frontier(&node).await;
    assert!(repaired.catalog_pins <= caches.retain.get());
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let restarted = Node::start(cfg).await.unwrap();
    assert!(restarted.snapshot_seq.is_some(), "real publication resumes after the fault");
    restarted.stop().await;
}

#[tokio::test]
async fn failed_reference_reconciliation_keeps_old_pins_before_any_gc_or_pointer_allocation() {
    let node = Node::start(configuration("paired-pin-release-barrier")).await.unwrap();
    // History catches up after each command, so no capture finds the previous one in flight.
    for n in 0..9 {
        node.submit(deposit(1, &format!("barrier-{n}"))).await;
        at_frontier(&node).await;
    }
    let before = at_frontier(&node).await;
    assert!(before.history_pins >= 2, "multiple actual retained roots exist");
    let caches = node.cfg.snapshots.as_ref().unwrap();
    let heads = head::candidates_readonly(caches).unwrap();
    assert_eq!(heads.len(), 2);
    // Preserve every byte while modeling metadata whose removal has not passed reconciliation.
    let saved: Vec<_> = heads.iter().map(|path| (path.clone(), path.with_extension("json.preserved"))).collect();
    for (path, saved) in &saved {
        fs::rename(path, saved).unwrap();
    }
    let journal = caches.dir.join("authenticated-generations.v1");
    let journal_saved = caches.dir.join("authenticated-generations.v1.preserved");
    fs::rename(&journal, &journal_saved).unwrap();
    fs::create_dir(&journal).unwrap();
    for n in 0..12 {
        node.submit(deposit(1, &format!("barrier-held-{n}"))).await;
    }
    let failed = at_frontier(&node).await;
    assert_eq!(failed.history_pins, before.history_pins, "failed barrier cannot release or allocate root pins");
    assert_eq!(failed.catalog_pins, before.catalog_pins, "uncertain retained data remains protected");
    fs::remove_dir(&journal).unwrap();
    fs::rename(&journal_saved, &journal).unwrap();
    for (path, saved) in saved {
        fs::rename(saved, path).unwrap();
    }
    for _ in 0..6 {
        node.submit(Command::Tick).await;
    }
    assert!(at_frontier(&node).await.last_entry > failed.last_entry);
    node.stop().await;
}

/// Failed captures at successive checkpoints are one `snapshot_write_failed` episode, each naming
/// its checkpoint's seq in the page context, and the first capture stored afterwards resolves it once.
#[tokio::test]
async fn failed_captures_across_checkpoints_are_one_alert_episode_until_one_is_stored() {
    keep_callsites_interested();
    let (layer, router) = alerts::channel(Routes::default(), Arc::new(Metrics::new()));
    let _collector = set_default(registry().with(layer.with_filter(filter_fn(is_alert_callsite))));
    let node = Node::start(configuration("paired-write-alert-episode")).await.unwrap();
    // A directory under a cache staging name fails the cleanup that precedes every cache write.
    let blocked_stage = node.cfg.snapshots.as_ref().unwrap().dir.join("99999999999999999999-0000000000.json.tmp");
    fs::create_dir_all(&blocked_stage).unwrap();
    // History catches up after each command, so no capture finds the previous one in flight.
    for n in 0..9 {
        node.submit(deposit(1, &format!("unstored-{n}"))).await;
        at_frontier(&node).await;
    }
    fs::remove_dir(&blocked_stage).unwrap();
    for _ in 0..6 {
        node.submit(Command::Tick).await;
        at_frontier(&node).await;
    }
    node.stop().await;
    let lines: Vec<_> = router.queued().into_iter().filter(|line| line.name == "snapshot_write_failed").collect();
    let (firing, resolved): (Vec<_>, Vec<_>) = lines.iter().partition(|line| line.severity.is_some());
    assert!(firing.len() >= 2, "captures failed at several checkpoints: {lines:#?}");
    assert!(firing.iter().all(|line| line.key() == firing[0].key()), "one episode key: {lines:#?}");
    let contexts: BTreeSet<_> = firing.iter().map(|line| line.context.as_str()).collect();
    assert_eq!(contexts.len(), firing.len(), "each failure names its checkpoint: {lines:#?}");
    assert!(contexts.iter().all(|context| context.contains("seq=")), "{lines:#?}");
    assert_eq!(resolved.len(), 1, "the first stored capture resolves the episode once: {lines:#?}");
    assert_eq!(resolved[0].key(), firing[0].key());
    assert_eq!(lines.last(), Some(resolved[0]), "the resolve follows every failure: {lines:#?}");
}

/// Observes the actual blocking owner after every physical entry in the producer's fixed frontier.
async fn at_frontier(node: &Node) -> crate::history::runtime::TestState {
    let target = node.manifest.borrow().flushed;
    timeout(Duration::from_secs(30), async {
        loop {
            let state = node.sealing.inspect().await.unwrap();
            if state.last_entry >= target {
                return state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("supervised history owner reaches the actual published frontier")
}
