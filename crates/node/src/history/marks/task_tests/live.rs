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
        run::Live::new(node.take_marks()),
        Default::default(),
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

/// A daily snapshot's freeze is a barrier in the reports, however long the task held them, as a backfill or a
/// catch-up holds them: its frozen copy holds the marks received before it and none received after, which reach the
/// cache once the copy is written.
#[tokio::test]
async fn a_freeze_copies_the_marks_before_it_and_none_after() {
    use crate::history::marks::{Freeze, Report};
    let cache = scratch_dir("marks-freeze-barrier");
    let store = Store::open_in(&cache, Retentions::defaults(), [0x42; 32]).unwrap();
    let reader = store.reader();
    let (samples, received) = tokio::sync::mpsc::channel(8);
    let mut live = run::Live::new(received);
    let offset = crate::log::Offset { seq: 7, index: 1 };
    let (reply, anchors) = std::sync::mpsc::sync_channel(1);
    let ts = 1_792_489_800_000;
    live.take(Report::Sample((1, ts, "67000".parse().unwrap())));
    live.take(Report::Freeze(Freeze { offset, reply }));
    live.take(Report::Sample((1, ts + 2_000, "67100".parse().unwrap())));
    drop(samples);
    let (sequencer, _) = tokio::sync::mpsc::channel(1);
    run::run(store, sequencer.downgrade(), None, BTreeMap::new(), live, Default::default()).await.unwrap();

    assert!(anchors.try_recv().is_ok(), "the freeze's anchors reached the snapshot");
    let copy = crate::history::marks::store::frozen_path(&cache, offset);
    let frozen: Vec<i64> = rusqlite::Connection::open(&copy)
        .unwrap()
        .prepare("SELECT bucket FROM marks WHERE market = 1 AND resolution = 0 ORDER BY bucket")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(frozen, [ts], "the copy holds the mark before the freeze alone");
    let seconds = reader.page(1, Resolution::S1, i64::MIN, i64::MAX, 100, 100).await.unwrap();
    assert_eq!(seconds.entries.len(), 2, "both marks reach the cache");
}
