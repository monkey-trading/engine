//! A frozen copy of the cache at a daily snapshot's checkpoint: written beside the cache with the anchors it holds,
//! the newest few kept, and taken into an empty cache only when every chain in it is the one those anchors name.

use super::*;
use crate::history::marks::store::Frozen;
use crate::history::marks::store::frozen::{KEPT, frozen_path};
use crate::log::Offset;

const BASE: i64 = 1_792_489_860_000;

/// A frozen copy imports into an empty cache as the cache it was taken from serves it, every resolution, pinned
/// rows and provider-seeded rows included, with its backfill markers, and the import goes on taking marks.
#[tokio::test]
async fn a_frozen_copy_imports_as_the_cache_it_was_taken_from() {
    let (path, mut store) = fresh_store_at("marks-frozen-import", Retentions::defaults());
    fill(&mut store);
    let frozen = store.freeze(Offset { seq: 9, index: 1 }).unwrap();
    assert!(frozen.pairs.iter().any(|pair| !pair.pins.is_empty()), "the cache pins a rewritten row: {frozen:?}");
    let copy = frozen_path(path.parent().unwrap(), Offset { seq: 9, index: 1 });

    let mut imported = fresh_store("marks-frozen-into", Retentions::defaults());
    let rows = imported.import_frozen(&copy, &frozen).unwrap();
    assert!(rows > 0);
    assert_eq!(every_page(&imported).await, every_page(&store).await);
    assert_eq!(imported.backfilled().unwrap(), store.backfilled().unwrap(), "the backfill markers");
    let later = [(1, BASE + 300_000, fixed("67400")), (2, BASE + 300_000, fixed("3400"))];
    imported.observe(&later).unwrap();
    store.observe(&later).unwrap();
    assert_eq!(every_page(&imported).await, every_page(&store).await, "the import goes on as the cache does");
}

/// A frozen copy the host rolled back to an earlier one of the same cache is refused, though every row in it is
/// authentic, and the cache it would go into is left empty.
#[tokio::test]
async fn a_rolled_back_frozen_copy_is_refused() {
    let (path, mut store) = fresh_store_at("marks-frozen-rollback", Retentions::defaults());
    fill(&mut store);
    let (earlier, later) = (Offset { seq: 9, index: 1 }, Offset { seq: 12, index: 1 });
    store.freeze(earlier).unwrap();
    store.observe(&[(1, BASE + 300_000, fixed("67400"))]).unwrap();
    let frozen = store.freeze(later).unwrap();
    let dir = path.parent().unwrap();
    std::fs::copy(frozen_path(dir, earlier), frozen_path(dir, later)).unwrap();

    let mut imported = fresh_store("marks-frozen-rollback-into", Retentions::defaults());
    let error = format!("{:#}", imported.import_frozen(&frozen_path(dir, later), &frozen).unwrap_err());
    assert!(error.contains("head is not the one the daily snapshot anchors"), "{error}");
    assert!(every_page(&imported).await.iter().all(|page| page.entries.is_empty()), "the refusal left rows");
}

/// A frozen copy with a row deleted, or with an earlier authentic version of a row the cache pinned, is refused.
#[tokio::test]
async fn a_frozen_copy_with_a_row_deleted_or_restored_is_refused() {
    let (path, mut store) = fresh_store_at("marks-frozen-altered", Retentions::defaults());
    let marks = [(1, BASE - 60_000, fixed("66900")), (1, BASE, fixed("67000")), (1, BASE + 60_000, fixed("67100"))];
    store.observe(&marks).unwrap();
    let (earlier, later) = (Offset { seq: 3, index: 1 }, Offset { seq: 5, index: 1 });
    store.freeze(earlier).unwrap();
    // A late mark rewrites a minute below the head, which the cache pins.
    store.observe(&[(1, BASE + 30_000, fixed("66000")), (1, BASE + 120_000, fixed("67200"))]).unwrap();
    let frozen = store.freeze(later).unwrap();
    let dir = path.parent().unwrap();
    let copy = frozen_path(dir, later);
    let pristine = std::fs::read(&copy).unwrap();
    let alter = |sql: &str, params: &[&dyn rusqlite::ToSql]| {
        std::fs::write(&copy, &pristine).unwrap();
        Connection::open(&copy).unwrap().execute(sql, params).unwrap();
    };
    let refused = |name: &str, said: &str| {
        let mut imported = fresh_store(name, Retentions::defaults());
        let error = format!("{:#}", imported.import_frozen(&copy, &frozen).unwrap_err());
        assert!(error.contains(said), "{name}: {error}");
    };

    alter("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", &[&(BASE + 60_000)]);
    refused("marks-frozen-deleted", "chain is broken");

    // The minute's version before the late mark, from the earlier copy, is authentic but not the pinned one.
    let old: (String, String, String, String, Vec<u8>) = Connection::open(frozen_path(dir, earlier))
        .unwrap()
        .query_row(
            "SELECT open, high, low, close, tag FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1",
            [BASE],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap();
    alter(
        "UPDATE marks SET open = ?2, high = ?3, low = ?4, close = ?5, tag = ?6
         WHERE market = 1 AND resolution = 1 AND bucket = ?1",
        &[&BASE, &old.0, &old.1, &old.2, &old.3, &old.4],
    );
    refused("marks-frozen-restored", "not its pinned version");

    std::fs::write(&copy, &pristine).unwrap();
    let other = Frozen { epoch: [7; 16], ..frozen.clone() };
    let mut imported = fresh_store("marks-frozen-epoch", Retentions::defaults());
    let error = format!("{:#}", imported.import_frozen(&copy, &other).unwrap_err());
    assert!(error.contains("run nonce"), "{error}");
}

/// The newest `KEPT` frozen copies stay beside the cache, and the older ones go.
#[test]
fn the_newest_frozen_copies_are_kept() {
    let (path, mut store) = fresh_store_at("marks-frozen-kept", Retentions::defaults());
    let offsets: Vec<Offset> = (1..=KEPT as u64 + 2).map(|seq| Offset { seq: seq * 10, index: 1 }).collect();
    for (n, offset) in offsets.iter().enumerate() {
        store.observe(&[(1, BASE + n as i64 * 1_000, fixed("67000"))]).unwrap();
        store.freeze(*offset).unwrap();
    }
    let dir = path.parent().unwrap();
    let held: Vec<bool> = offsets.iter().map(|offset| frozen_path(dir, *offset).exists()).collect();
    let kept = offsets.len() - KEPT;
    assert_eq!(held, [vec![false; kept], vec![true; KEPT]].concat());
}

// ── Helpers ──

/// Marks in two markets over five minutes, a late mark that rewrites a minute below the head, and a provider seed
/// below them with its backfill marker.
fn fill(store: &mut Store) {
    store
        .observe(&[
            (1, BASE, fixed("67000")),
            (2, BASE, fixed("3000")),
            (1, BASE + 60_000, fixed("67100")),
            (1, BASE + 120_000, fixed("67200")),
            (2, BASE + 180_000, fixed("3100")),
        ])
        .unwrap();
    store.observe(&[(1, BASE + 30_000, fixed("66900"))]).unwrap();
    let seed = Seed {
        market: 2,
        resolution: Resolution::M1,
        ts: BASE - 60_000,
        open: fixed("2990"),
        high: fixed("2995"),
        low: fixed("2985"),
        close: fixed("2992"),
    };
    store.seed(&[seed], BASE + 180_000).unwrap();
    let coverage = Coverage { depths: (1, 2, 3), retentions: (4, 5, 6, None) };
    store.mark_backfilled(2, "coinbase", "ETH-USD", BASE, coverage).unwrap();
}

/// Every page of markets 1 and 2 at every resolution.
async fn every_page(store: &Store) -> Vec<MarksPage> {
    let mut pages = Vec::new();
    for market in [1, 2] {
        for resolution in Resolution::ALL {
            pages.push(store.reader().page(market, resolution, i64::MIN, i64::MAX, 10_000, 10_000).await.unwrap());
        }
    }
    pages
}
