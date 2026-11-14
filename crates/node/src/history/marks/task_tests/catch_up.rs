//! A member's catch-up beside the sequencer: the marks task keeps taking live samples while its
//! blocking thread merges what the replica folded, so the sequencer's queue drops none.

use std::collections::BTreeMap;

use engine_core::Fixed;
use tokio::sync::mpsc;

use crate::history::marks::fold::MarkFold;
use crate::history::marks::run;
use crate::history::marks::store::Store;
use crate::history::marks::{Resolution, Retentions};
use crate::test_support::scratch_dir;

/// While the catch-up merges a day of one-second marks in four markets, and while the backlog it left
/// is written, the sequencer's sends, sixteen a millisecond, never find the queue full, though they
/// far outnumber it; every one is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_catch_up_drops_no_live_sample_while_it_merges_a_large_fold() {
    // One-second buckets kept for ten days, so however many samples a slow run sends, none expires.
    let retentions = Retentions { s1: 10 * 86_400_000, ..Retentions::defaults() };
    let store = Store::open_in(&scratch_dir("marks-catch-up"), retentions, [0x42; 32]).unwrap();
    store.defer();
    let reader = store.reader();
    let base = 1_792_489_800_000;
    let mut fold = MarkFold::new(retentions);
    for market in 1..=4 {
        for second in 0..86_400 {
            fold.sample(market, base + second * 1_000, mark(second));
        }
    }
    let (tx, rx) = mpsc::channel(run::QUEUE);
    let live = run::Live { samples: rx, buffered: Vec::new(), flushed_at: tokio::time::Instant::now() };
    let catch_up = run::CatchUp { prior: None, folded: Some(fold) };
    let (sequencer, _requests) = mpsc::channel(1);
    let task = tokio::spawn(run::run(store, sequencer.downgrade(), None, BTreeMap::new(), live, catch_up));
    // Sends until twice the queue has gone after the catch-up ended, while its backlog is written.
    let (mut sent, mut while_catching_up, mut after) = (0_i64, 0, 0);
    while after < 2 * run::QUEUE {
        // As the sequencer sends: a full queue drops the sample.
        let sample = (5, base + 86_400_000 + sent * 1_000, mark(sent));
        tx.try_send(sample).unwrap_or_else(|e| panic!("sample {sent} was dropped: {e}"));
        match reader.ready() {
            true => after += 1,
            false => while_catching_up += 1,
        }
        sent += 1;
        if sent % 16 == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }
    assert!(while_catching_up > run::QUEUE, "the catch-up ended before the queue could fill: {while_catching_up}");
    drop(tx);
    task.await.unwrap().unwrap();
    assert!(reader.ready(), "the catch-up never finished");
    let limit = usize::try_from(sent).unwrap() + 1;
    let live = reader.page(5, Resolution::S1, i64::MIN, i64::MAX, limit, limit).await.unwrap();
    assert_eq!(live.entries.len() as i64, sent, "a live sample was not written");
    let folded = reader.page(1, Resolution::M1, i64::MIN, i64::MAX, 2_000, 2_000).await.unwrap();
    assert_eq!(folded.entries.len(), 1_440, "the fold was not merged whole");
}

/// A mark that moves with `i`.
fn mark(i: i64) -> Fixed {
    Fixed::from_int(67_000 + i % 100)
}
