//! The dev scenario of a bounded account history index (`scripts/dev-history-retention.sh`): a node with
//! `[snapshots]` and a short `history.retention` takes deposits on a test clock for several retentions, two
//! recovery caches pinning its recent versions as a node's do. Once its first events expire, the index's file
//! stops growing while the log keeps growing, and every page states where its history begins and holds every
//! logged event from then on and none before.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engine_core::types::Millis;

use crate::clock::test_clock::TestClock;
use crate::clock::{Clock, SystemClock};
use crate::config::NodeConfig;
use crate::history::PageQuery;
use crate::log::CheckpointDomain;
use crate::node::paired_tests::harness::{Node, configuration};
use crate::snapshot;
use crate::test_support::{TRADER, deposit, logged_history};

/// The index's retention, ten minutes of sequenced time.
const RETENTION: Duration = Duration::from_secs(600);
/// A minute of sequenced time, the step the clock takes between bursts of deposits.
const MINUTE: Duration = Duration::from_secs(60);
/// Deposits sequenced each minute: a retention holds about four times the commands between the owner's
/// collections, so it collects several times in each.
const PER_MINUTE: usize = 400;
/// Retentions the scenario runs: the first fills the index, the later ones expire as much as they add.
const RETENTIONS: usize = 6;

#[tokio::test(flavor = "multi_thread")]
async fn dev_scenario_history_retention() {
    let mut cfg = configuration("paired-retention");
    cfg.history.retention = RETENTION;
    let section = cfg.snapshots.as_mut().unwrap();
    (section.interval, section.retain) = (MINUTE, 2.try_into().unwrap());
    let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
    let node = Node::start_at(cfg, clock.clone()).await.unwrap();
    let minutes = RETENTION.as_secs() / MINUTE.as_secs();
    let mut sizes = Vec::new();
    for retention in 0..RETENTIONS {
        // The index's file saws between collections: it grows by what each minute adds and drops when a
        // collection gives back the pages the oldest caches' versions pinned, so one reading at the end of a
        // retention lands anywhere on the saw, by where scheduling put the collections. The largest it
        // reaches in the retention is what retention bounds, read once the index holds each minute.
        let mut peak = 0;
        for minute in 0..minutes {
            // The clock moves before each minute's deposits, so nothing sequenced after the last of them is later.
            clock.advance(MINUTE);
            for n in 0..PER_MINUTE {
                node.submit(deposit(1, &format!("r{retention}-m{minute}-{n}"))).await;
            }
            // A node skips a capture while another is being sealed, and the caches left pin more versions, so
            // each minute's cache is awaited before the index is read.
            node.sealing.idle().await;
            node.history_caught_up().await;
            if let Some(store) = history_store(&node.cfg) {
                peak = peak.max(std::fs::metadata(store).unwrap().len());
            }
        }
        sizes.push((peak, log_bytes(&node.cfg.node.log_dir)));
        println!(
            "SCENARIO retention={retention} index_peak_bytes={} log_bytes={}",
            sizes[retention].0, sizes[retention].1
        );
    }
    // The log grows by the same deposits each retention, while the index, once its first events expire, holds
    // one retention's of them. The largest it reaches in the later retentions is held to the largest of the
    // second and third: from the third retention to the last the log doubles, while the index's high-water
    // mark grows by under a quarter.
    let high = |range: std::ops::Range<usize>| sizes[range].iter().map(|(index, _)| *index).max().unwrap();
    let (settled, later) = (high(1..3), high(3..RETENTIONS));
    assert!(sizes[RETENTIONS - 1].1 > sizes[2].1 * 3 / 2, "the log keeps growing: {sizes:?}");
    assert!(later < settled + settled / 4, "the index's file stays bounded by its retention: {sizes:?}");
    let page = page(&node).await;
    let begins = newest_ts(&node.cfg.node.log_dir) - Millis::try_from(RETENTION.as_millis()).unwrap();
    assert_eq!(page.retained_from_ms, Some(begins), "the page states where its history begins");
    let held: Vec<_> = logged_history(&node.cfg.node.log_dir, TRADER).into_iter().filter(|e| e.ts >= begins).collect();
    assert!(!held.is_empty() && held.len() < RETENTIONS * PER_MINUTE * minutes as usize / 2);
    assert_eq!(page.events, held, "every logged event from where history begins, and none before");
    node.stop().await;
}

// ── Helpers ──

/// Every page of the trader's history from the newest, joined, and the first page's statement of where the
/// history begins.
async fn page(node: &Node) -> crate::history::Page {
    let (mut events, mut before, mut begins) = (Vec::new(), None, None);
    loop {
        let query = PageQuery { account: TRADER, kind: None, before, after: None, limit: 200 };
        let page = node.reader.page(query).await.unwrap().expect("history serves");
        begins = begins.or(Some(page.retained_from_ms));
        events.extend(page.events);
        before = page.next;
        if before.is_none() {
            return crate::history::Page { events, next: None, retained_from_ms: begins.flatten() };
        }
    }
}

/// The file of the history index the newest cache pairs with, once a cache exists.
fn history_store(cfg: &NodeConfig) -> Option<PathBuf> {
    let section = cfg.snapshots.as_ref().unwrap();
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() };
    let newest = snapshot::candidates_readonly(section).unwrap().into_iter().next()?;
    let (payload, _) = snapshot::read(section, &newest, domain.chain_id, domain.vault).unwrap();
    let pair = payload.pair.expect("a paired cache");
    let statement = snapshot::head::verify_certificate(section, &pair.history, domain).unwrap();
    Some(statement.store.path(&section.dir))
}

/// The bytes of the log's files in `dir`.
fn log_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir).unwrap().filter_map(|entry| entry.ok()?.metadata().ok()).map(|metadata| metadata.len()).sum()
}

/// The sequenced time of the newest command in the log in `dir`.
fn newest_ts(dir: &Path) -> Millis {
    crate::test_support::log_entries(dir)
        .into_iter()
        .filter_map(|entry| match entry {
            crate::log::LogEntry::Command(command) => Some(command.sequenced.ts),
            _ => None,
        })
        .max()
        .expect("the log holds a command")
}
