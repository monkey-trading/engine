//! Recovery seeding: the replay's marks sink fills the cache from a short log, and reseeding the
//! same log through a fresh cache gives the same series — the cache is derived, and either the
//! log or the provider rebuilds it.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use engine_core::types::AccountId;

use super::{priced_node, prices};
use crate::clock::Clock;
use crate::history::marks::store::Store;
use crate::history::marks::{Provenance, Resolution, Retentions, Seeder};
use crate::log::CheckpointDomain;
use crate::sequencer::Sequencer;
use crate::tee::EnclaveKey;
use crate::test_support::scratch_dir;

#[tokio::test]
async fn recovery_seeds_the_cache_and_reseeding_matches() {
    let dir = scratch_dir("marks-recovery-seed");
    let (node, clock) = priced_node(&dir);
    // Two price moves two seconds apart: two 1s buckets in one 1m bucket.
    node.submit(prices(1, clock.now_ms(), "67000")).await;
    clock.advance(Duration::from_secs(2));
    node.submit(prices(1, clock.now_ms(), "67100")).await;
    node.stop().await;

    let seeded = seed_from_log(&dir);
    let reader = seeded.reader();
    let seconds = reader.page(1, Resolution::S1, i64::MIN, i64::MAX, 100, 100).await.unwrap();
    assert_eq!(seconds.entries.len(), 2);
    assert_eq!(seconds.entries[0].open, "67000".parse().unwrap());
    assert_eq!(seconds.entries[1].close, "67100".parse().unwrap());
    assert!(seconds.entries.iter().all(|entry| entry.provenance == Provenance::LogDerived));
    let minute = reader.page(1, Resolution::M1, i64::MIN, i64::MAX, 100, 100).await.unwrap();
    assert_eq!(minute.entries.len(), 1);
    assert_eq!(minute.entries[0].high, "67100".parse().unwrap());
    assert_eq!(minute.entries[0].low, "67000".parse().unwrap());

    // A fresh cache over the same log seeds the same series: the cache is rebuildable state.
    let reseeded = seed_from_log(&dir);
    let again = reseeded.reader().page(1, Resolution::S1, i64::MIN, i64::MAX, 100, 100).await.unwrap();
    assert_eq!(again.entries, seconds.entries);
}

/// Opens a fresh cache and seeds it through the replay's marks sink, as recovery does.
fn seed_from_log(dir: &std::path::Path) -> Store {
    let store = Store::open_in(&scratch_dir("marks-recovery-cache"), Retentions::defaults(), [0x42; 32]).unwrap();
    let shared = Rc::new(RefCell::new(store));
    let failed = Rc::new(std::cell::Cell::new(false));
    let reset_done = Rc::new(std::cell::Cell::new(false));
    let mut seeder = Seeder::new(shared.clone(), failed, reset_done);
    Sequencer::read_log_with_authorities(
        dir,
        Some(CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) }),
        Some(AccountId(EnclaveKey::test_key().address())),
        None,
        Default::default(),
        |_| {},
        Some(Box::new(move |market, ts, mark| seeder.sample((market, ts, mark)))),
    )
    .unwrap();
    match Rc::try_unwrap(shared) {
        Ok(cell) => cell.into_inner(),
        Err(_) => unreachable!("the seeder is dropped with the replay"),
    }
}
