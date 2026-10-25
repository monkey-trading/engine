//! Paired restart through node startup: exact seals, fallback, and full rebuild.

use std::fs;

use engine_core::command::Command;

use crate::authenticated::Store;
use crate::history::PageQuery;
use crate::log::catalog_head::CatalogDomain;
use crate::node::paired_tests::harness::{Node, configuration};
use crate::snapshot;
use crate::test_support::{TRADER, deposit, logged_history, withdraw};

#[tokio::test]
async fn paired_node_drains_exact_checkpoint_seals_and_restarts_with_complete_history() {
    let node = Node::start(configuration("paired-node-roundtrip")).await.unwrap();
    assert_eq!(node.snapshot_seq, None, "first boot is log-derived migration");
    for n in 0..17 {
        node.submit(deposit(3, &format!("paired-deposit-{n}"))).await;
        node.submit(withdraw(1)).await;
    }
    let (cfg, reader) = node.stop().await;
    let expected = logged_history(&cfg.node.log_dir, TRADER);
    let query = || PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
    assert_eq!(reader.page(query()).await.unwrap().unwrap().events, expected);
    let (payload, _) = snapshot::read(
        cfg.snapshots.as_ref().unwrap(),
        &snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap()[0],
        cfg.custody.chain_id,
        cfg.custody.vault.parse().unwrap(),
    )
    .unwrap();
    let pair = payload.pair.as_ref().expect("Node must never publish engine-only caches");
    let statement = snapshot::head::verify_certificate(
        cfg.snapshots.as_ref().unwrap(),
        &pair.history,
        crate::log::CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() },
    )
    .unwrap();
    assert_eq!(statement.head.last_seq(), Some(payload.checkpoint.seq), "H acknowledges exact C");
    assert_eq!(statement.head.command_prefix, payload.context.command_prefix);
    assert_eq!(pair.catalog.checkpoint.checkpoint.seq, payload.checkpoint.seq);
    drop(reader);
    let node = Node::start(cfg).await.unwrap();
    assert!(node.snapshot_seq.is_some(), "real Node startup must select paired fast recovery");
    let (cfg, reader) = node.stop_after_signing("after-paired-restart").await;
    assert_eq!(reader.page(query()).await.unwrap().unwrap().events, logged_history(&cfg.node.log_dir, TRADER));
}

#[tokio::test]
async fn corrupt_latest_pair_falls_back_and_all_pairs_missing_rebuilds_exact_log_history() {
    let node = Node::start(configuration("paired-node-fallback")).await.unwrap();
    for n in 0..25 {
        node.submit(deposit(1, &format!("fallback-{n}"))).await;
    }
    // No capture is in flight at shutdown, so the final checkpoint is cached and an earlier pair
    // exists to fall back to.
    node.history_caught_up().await;
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let caches = cfg.snapshots.as_ref().unwrap();
    let candidates = snapshot::candidates_readonly(caches).unwrap();
    assert!(candidates.len() > 1, "earlier pair exists for actual fallback");
    fs::write(&candidates[0], b"corrupt signed envelope").unwrap();
    let node = Node::start(cfg).await.unwrap();
    assert!(node.snapshot_seq.is_some());
    let (cfg, reader) = node.stop_after_signing("after-fallback").await;
    let expected = logged_history(&cfg.node.log_dir, TRADER);
    let query = || PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
    assert_eq!(reader.page(query()).await.unwrap().unwrap().events, expected);
    drop(reader);
    for path in snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap() {
        fs::write(path, b"{").unwrap();
    }
    let node = Node::start(cfg).await.unwrap();
    assert_eq!(node.snapshot_seq, None, "global H/T alone cannot replace engine C or required Work");
    let (_, reader) = node.stop().await;
    assert_eq!(reader.page(query()).await.unwrap().unwrap().events, expected);
}

/// A crash after the history owner commits commands but before it signs their head leaves later
/// versions in the store. Restart still uses the paired cache, discards those versions, and
/// keeps the same history generation instead of rebuilding it.
#[tokio::test]
async fn restart_after_unsigned_history_versions_resumes_from_the_newest_signed_head() {
    let node = Node::start(configuration("paired-unsigned-history")).await.unwrap();
    for n in 0..10 {
        node.submit(deposit(2, &format!("unsigned-{n}"))).await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let (id, root) = newest_history_head(&cfg);
    let store = Store::open_existing(id.path(&cfg.snapshots.as_ref().unwrap().dir)).unwrap();
    store.enable_writes().unwrap();
    store.update(&root, vec![(b"crash".to_vec(), Some(b"unsigned".to_vec()))]).unwrap();
    drop(store);

    let node = Node::start(cfg).await.unwrap();
    assert!(node.snapshot_seq.is_some(), "unsigned history versions must not force full replay");
    node.submit(deposit(2, "after-unsigned")).await;
    let (cfg, reader) = node.stop().await;
    let expected = logged_history(&cfg.node.log_dir, TRADER);
    let query = PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
    assert_eq!(reader.page(query).await.unwrap().unwrap().events, expected);
    assert_eq!(newest_history_head(&cfg).0, id, "the generation resumed instead of being rebuilt");
}

/// The catalog mirror: a crash after a catalog commit but before its head was signed. Restart still
/// uses the paired cache, discards that version after the preflight, and keeps the generation.
#[tokio::test]
async fn restart_after_an_unsigned_catalog_version_resumes_from_the_newest_signed_catalog_head() {
    let node = Node::start(configuration("paired-unsigned-catalog")).await.unwrap();
    let catalog = node.sealing.catalog_store;
    for n in 0..10 {
        node.submit(deposit(2, &format!("unsigned-catalog-{n}"))).await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let caches = cfg.snapshots.as_ref().unwrap();
    let vault: engine_core::types::AccountId = cfg.custody.vault.parse().unwrap();
    let domain = CatalogDomain { chain_id: cfg.custody.chain_id, vault: vault.0, log_id: catalog.catalog_namespace() };
    let tip = snapshot::catalog_heads::read_latest(caches, catalog, &domain).unwrap();
    let store = Store::open_existing(catalog.path(&caches.dir)).unwrap();
    store.enable_writes().unwrap();
    store.update(&tip.catalog().root, vec![(b"crash".to_vec(), Some(b"unsigned".to_vec()))]).unwrap();
    drop(store);

    let node = Node::start(cfg).await.unwrap();
    assert!(node.snapshot_seq.is_some(), "an unsigned catalog version must not force full replay");
    assert_eq!(node.sealing.catalog_store, catalog, "the catalog generation resumed instead of being rebuilt");
    let (cfg, reader) = node.stop_after_signing("after-unsigned-catalog").await;
    let query = PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
    assert_eq!(reader.page(query).await.unwrap().unwrap().events, logged_history(&cfg.node.log_dir, TRADER));
}

/// A kill mid-transaction leaves a hot rollback journal beside the history and catalog stores.
/// Recovery reads each store as it was, the preflight runs, and only activation rolls the journals
/// back, so restart still uses the paired cache and keeps both generations.
#[tokio::test]
async fn restart_after_interrupted_store_transactions_uses_the_paired_cache() {
    let node = Node::start(configuration("paired-hot-journals")).await.unwrap();
    let catalog = node.sealing.catalog_store;
    for n in 0..10 {
        node.submit(deposit(2, &format!("hot-journal-{n}"))).await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let dir = cfg.snapshots.as_ref().unwrap().dir.clone();
    let (history, _) = newest_history_head(&cfg);
    let journal = |id: snapshot::head::StoreId| {
        let mut path = id.path(&dir).into_os_string();
        path.push("-journal");
        std::path::PathBuf::from(path)
    };
    for id in [history, catalog] {
        crate::authenticated::tests::interrupt_transaction(&id.path(&dir));
        assert!(journal(id).exists());
    }

    let node = Node::start(cfg).await.unwrap();
    assert!(node.snapshot_seq.is_some(), "an interrupted store transaction must not force full replay");
    assert_eq!(node.sealing.catalog_store, catalog, "the catalog generation resumed");
    assert!(!journal(history).exists() && !journal(catalog).exists(), "activation rolled both journals back");
    let (cfg, reader) = node.stop_after_signing("after-hot-journals").await;
    assert_eq!(newest_history_head(&cfg).0, history, "the history generation resumed");
    let query = PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
    assert_eq!(reader.page(query).await.unwrap().unwrap().events, logged_history(&cfg.node.log_dir, TRADER));
}

/// Catalog retirement runs once per sealed checkpoint and keeps up: the backlog of retirement
/// receipts does not grow as checkpoints accumulate.
#[tokio::test]
async fn catalog_retirement_backlog_stays_bounded_as_checkpoints_accumulate() {
    let mut cfg = configuration("paired-catalog-retirement");
    let mut backlog = Vec::new();
    for round in 0..2 {
        let node = Node::start(cfg).await.unwrap();
        let (manifest, catalog) = (node.manifest.clone(), node.sealing.catalog_store);
        for n in 0..30 {
            node.submit(deposit(1, &format!("retire-{round}-{n}"))).await;
            // Retained caches pin catalog versions, so the backlog depends on which checkpoints were
            // captured. History catches up after each command, so no capture is skipped.
            node.history_caught_up().await;
        }
        let (next, reader) = node.stop().await;
        drop(reader);
        cfg = next;
        let root = manifest.borrow().catalog.as_ref().unwrap().certified().root.clone();
        let store = Store::open_existing(catalog.path(&cfg.snapshots.as_ref().unwrap().dir)).unwrap();
        backlog.push(store.pending_receipts(&root).unwrap());
    }
    assert!(backlog[1] <= backlog[0] + 2, "retirement must keep pace with checkpoints: {backlog:?}");
}

/// A completed capture signs a history head, which used to reset the only collection cadence, so
/// with checkpoints more frequent than it history never collected. The retirement backlog must stay
/// bounded while every checkpoint is captured.
#[tokio::test]
async fn history_retirement_keeps_pace_while_every_checkpoint_is_captured() {
    let mut cfg = configuration("paired-history-retirement");
    cfg.node.segment_commands = 100;
    cfg.node.checkpoint_every = 50;
    let node = Node::start(cfg).await.unwrap();
    // One history commit, and so one retirement receipt, per command.
    for _ in 0..1_100 {
        node.submit(Command::Tick).await;
        node.history_caught_up().await;
    }
    let backlog = node.sealing.inspect().await.unwrap().pending_receipts;
    assert!(backlog < 512, "history must collect while captures sign heads: {backlog} receipts queued");
    node.stop().await;
}

/// The store and root named by the newest history head that verifies.
fn newest_history_head(
    cfg: &crate::config::NodeConfig,
) -> (crate::snapshot::head::StoreId, crate::authenticated::Root) {
    let caches = cfg.snapshots.as_ref().unwrap();
    let domain =
        crate::log::CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() };
    let statement = snapshot::head::candidates_readonly(caches)
        .unwrap()
        .iter()
        .find_map(|path| snapshot::head::read(caches, path, domain).ok())
        .expect("a signed history head");
    (statement.store, statement.head.root)
}
