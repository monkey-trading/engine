//! A member's catch-up beside the sequencer: the marks task keeps taking live samples while its
//! blocking thread merges what the replica folded, so the sequencer's queue drops none.

use std::collections::BTreeMap;

use engine_core::Fixed;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::history::marks::fold::MarkFold;
use crate::history::marks::run;
use crate::history::marks::store::Store;
use crate::history::marks::{Report, Resolution, Retentions};
use crate::test_support::scratch_dir;

/// The most the test waits for the catch-up to end: a ceiling only a catch-up that never ends reaches.
const HANG: std::time::Duration = std::time::Duration::from_secs(600);

/// While the catch-up merges a day of one-second marks in four markets, the task takes more than a
/// queue's worth of the sequencer's live samples, and while the backlog is written after it every
/// later one; none is dropped and every one is written. The sender sends only as the task takes
/// samples from the queue, as the sequencer's rate stays below what its disk writes: a sender that
/// outpaced a slow disk would drop samples by design (see `run`), whatever `caught_up` does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_catch_up_drops_no_live_sample_while_it_merges_a_large_fold() {
    // One-second buckets kept for ten days, so however many samples a run sends, none expires.
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
    let live = run::Live::new(rx);
    let catch_up = run::CatchUp { prior: None, folded: Some(fold) };
    let (sequencer, _requests) = mpsc::channel(1);
    let task = tokio::spawn(run::run(store, sequencer.downgrade(), None, BTreeMap::new(), live, catch_up));
    // Twice the queue is sent while the merge runs, unless it ends first, and twice the queue after.
    let (mut sent, mut while_catching_up) = (0_i64, 0);
    while sent < 2 * run::QUEUE as i64 {
        // A free slot is the task having taken a sample, so a send stuck behind a task that takes
        // none ends only once the merge has, and is not counted as sent during it.
        let slot = tx.reserve().await.expect("the task takes samples");
        while_catching_up += i64::from(!reader.ready());
        slot.send(Report::Sample((5, base + 86_400_000 + sent * 1_000, mark(sent))));
        sent += 1;
    }
    assert!(
        while_catching_up > run::QUEUE as i64,
        "the task took no more than the queue while merging: {while_catching_up}"
    );
    // The merge's length is the disk's, so the wait is for the catch-up to end, bounded only against a hang.
    let ended = async {
        while !reader.ready() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    timeout(HANG, ended).await.expect("the catch-up ended");
    for _ in 0..2 * run::QUEUE {
        let sample = Report::Sample((5, base + 86_400_000 + sent * 1_000, mark(sent)));
        tx.send(sample).await.expect("the task takes samples");
        sent += 1;
    }
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
