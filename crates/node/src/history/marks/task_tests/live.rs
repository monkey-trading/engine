//! Live maintenance: the sequencer's mark reports flow through the marks task into the cache, and
//! the task flushes what it held and ends when the sequencer stops.

use std::collections::BTreeMap;
use std::time::Duration;

use super::{priced_node, prices};
use crate::clock::Clock;
use crate::history::marks::run;
use crate::history::marks::store::Store;
use crate::history::marks::{Provenance, Resolution, Retentions};
use crate::test_support::scratch_dir;

#[tokio::test]
async fn live_marks_reach_the_cache_and_flush_when_the_sequencer_stops() {
    let dir = scratch_dir("marks-live");
    let (mut node, clock) = priced_node(&dir);
    let store = Store::open_in(&scratch_dir("marks-live-cache"), Retentions::defaults(), [0x42; 32]).unwrap();
    let reader = store.reader();
    let task = tokio::spawn(run::run(
        store,
        node.tx.downgrade(),
        None,
        BTreeMap::new(),
        run::Live { samples: node.take_marks(), buffered: Vec::new(), flushed_at: tokio::time::Instant::now() },
    ));

    // Two price moves two seconds apart; then the shutdown drops every sender, and the marks
    // task flushes and ends.
    node.submit(prices(1, clock.now_ms(), "67000")).await;
    clock.advance(Duration::from_secs(2));
    node.submit(prices(1, clock.now_ms(), "67100")).await;
    node.stop().await;
    task.await.expect("marks task").expect("marks task");

    let seconds = reader.page(1, Resolution::S1, i64::MIN, i64::MAX, 100, 100).await.unwrap();
    assert_eq!(seconds.entries.len(), 2);
    assert_eq!(seconds.entries[0].open, "67000".parse().unwrap());
    assert_eq!(seconds.entries[1].close, "67100".parse().unwrap());
    assert!(seconds.entries.iter().all(|entry| entry.provenance == Provenance::LogDerived));
}
