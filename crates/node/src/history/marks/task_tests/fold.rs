//! A member's folded marks: merged into its cache once its node starts from the replica, every
//! bucket at every resolution equals what recovery's replay derives from the same log, whether the
//! member never held a turn or held one before another member's.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use engine_core::types::{AccountId, Millis};

use super::recovery::seed_from_log;
use super::{priced_node, prices};
use crate::clock::Clock;
use crate::history::marks::fold::MarkFold;
use crate::history::marks::store::Store;
use crate::history::marks::{MarkSample, Resolution, Retentions};
use crate::log::CheckpointDomain;
use crate::sequencer::replay::LiveReplay;
use crate::tee::EnclaveKey;
use crate::test_support::{log_entries, sample_around, scratch_dir};

#[tokio::test]
async fn folded_marks_merge_into_the_buckets_recovery_derives() {
    let dir = scratch_dir("marks-fold-log");
    let (node, clock) = priced_node(&dir);
    // Several marks a second, then across minutes, then past an hour.
    let mut mid = 67_000;
    for step in [400, 400, 400, 900, 61_000, 61_000, 30_000, 3_700_000, 500, 61_000] {
        node.submit(prices(1, clock.now_ms(), &mid.to_string())).await;
        mid += if step % 2 == 0 { 35 } else { -20 };
        clock.advance(Duration::from_millis(step));
        // Trusted time moves with the clock, so the jumps past the drift bound still stamp.
        node.tx.send(crate::sequencer::Request::TimeSample(sample_around(clock.as_ref()))).await.unwrap();
    }
    node.submit(prices(1, clock.now_ms(), &mid.to_string())).await;
    node.stop().await;
    let samples = replica_samples(&dir);
    let recovered = seed_from_log(&dir);

    // A first turn: the member never held, so its cache holds only what its replica folded.
    let first = fresh_cache("marks-fold-first");
    merge_fold(&first, &samples);
    assert_same(&first, &recovered, "a first turn").await;

    // A return: the member held the log's first part, its node reporting those marks live; another
    // member held the rest while it followed, its replica folding the whole log.
    let returning = fresh_cache("marks-fold-return");
    let held = samples.len() / 3;
    returning.borrow_mut().observe(&samples[..held]).unwrap();
    merge_fold(&returning, &samples);
    assert_same(&returning, &recovered, "a return after another member's turn").await;
}

/// The marks the replica of the log in `dir` applies, as a follower's build hands them to its fold.
fn replica_samples(dir: &std::path::Path) -> Vec<MarkSample> {
    let signer = AccountId(EnclaveKey::test_key().address());
    let domain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };
    let mut replica = LiveReplay::new(domain, [signer].into());
    let samples = Rc::new(RefCell::new(Vec::new()));
    let sink = samples.clone();
    replica.sample_marks_with(Box::new(move |market, ts, mark| sink.borrow_mut().push((market, ts, mark))));
    for entry in log_entries(dir) {
        replica.apply(&entry).unwrap();
    }
    drop(replica);
    Rc::try_unwrap(samples).unwrap().into_inner()
}

fn fresh_cache(name: &str) -> RefCell<Store> {
    RefCell::new(Store::open_in(&scratch_dir(name), Retentions::defaults(), [0x42; 32]).unwrap())
}

/// Folds `samples` as a following member does, and merges the fold as its node's start does.
fn merge_fold(store: &RefCell<Store>, samples: &[MarkSample]) {
    let mut fold = MarkFold::new(Retentions::defaults());
    for &(market, ts, mark) in samples {
        fold.sample(market, ts, mark);
    }
    let (buckets, newest) = fold.into_buckets().expect("the log moved marks");
    store.borrow_mut().observe_buckets(&buckets, newest).unwrap();
}

/// Every bucket of market 1 at every resolution is the same in both caches.
async fn assert_same(store: &RefCell<Store>, recovered: &Store, case: &str) {
    let reader = store.borrow().reader();
    for resolution in Resolution::ALL {
        let (from, to) = (Millis::MIN, Millis::MAX);
        let ours = reader.page(1, resolution, from, to, 1_000, 1_000).await.unwrap().entries;
        let theirs = recovered.reader().page(1, resolution, from, to, 1_000, 1_000).await.unwrap().entries;
        assert!(!theirs.is_empty(), "{case}: recovery derived no {resolution:?} bucket");
        assert_eq!(ours, theirs, "{case}: the {resolution:?} buckets differ from recovery's");
    }
}
