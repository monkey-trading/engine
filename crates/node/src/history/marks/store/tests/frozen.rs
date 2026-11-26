//! A frozen copy of the cache at a daily snapshot's checkpoint: written beside the cache with the anchors it holds and
//! the root over its rows, the newest few kept, and taken into an empty cache, under any key, only when every chain in
//! it is the one those anchors name and its rows are the ones the root covers.

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

/// A frozen copy imports into a cache under another marks key, as after a start under keys sealed afresh, on the root
/// the freeze found, and the import goes on taking marks as the cache does. The anchors of a snapshot written before
/// the root was carried, which decode with none, take the copy only under the key that wrote it.
#[tokio::test]
async fn a_frozen_copy_imports_under_another_key_on_its_root() {
    let (path, mut store) = fresh_store_at("marks-frozen-new-key", Retentions::defaults());
    fill(&mut store);
    let offset = Offset { seq: 9, index: 1 };
    let frozen = store.freeze(offset).unwrap();
    assert!(frozen.root.is_some(), "the freeze names no root: {frozen:?}");
    let (copy, served) = (frozen_path(path.parent().unwrap(), offset), every_page(&store).await);

    let mut imported = other_key_store("marks-frozen-new-key-into");
    assert!(imported.import_frozen(&copy, &frozen).unwrap() > 0);
    assert_eq!(every_page(&imported).await, served);
    let later = [(1, BASE + 300_000, fixed("67400")), (2, BASE + 300_000, fixed("3400"))];
    imported.observe(&later).unwrap();
    store.observe(&later).unwrap();
    assert_eq!(every_page(&imported).await, every_page(&store).await, "the import goes on as the cache does");

    let mut written = serde_json::to_value(&frozen).unwrap();
    assert!(written.as_object_mut().unwrap().remove("root").is_some(), "the root is not written: {written}");
    let earlier: Frozen = serde_json::from_value(written).unwrap();
    assert_eq!(earlier, Frozen { root: None, ..frozen.clone() });
    let mut refused = other_key_store("marks-frozen-new-key-earlier");
    let error = format!("{:#}", refused.import_frozen(&copy, &earlier).unwrap_err());
    assert!(error.contains("fails authentication"), "{error}");
    assert!(every_page(&refused).await.iter().all(|page| page.entries.is_empty()), "the refusal left rows");
    let mut same_key = fresh_store("marks-frozen-new-key-same", Retentions::defaults());
    same_key.import_frozen(&copy, &earlier).unwrap();
    assert_eq!(every_page(&same_key).await, served, "the key that wrote it takes it");
}

/// Under another marks key, where no row's tag can be checked, a copy in which the host changed a row, swapped two
/// rows' prices, or removed a row and relinked the chain around it, so every chain still meets its anchors, is refused
/// on the root alone, and the cache it would go into is left empty.
#[tokio::test]
async fn a_changed_reordered_or_removed_row_is_refused_on_the_root() {
    let (path, mut store) = fresh_store_at("marks-frozen-root", Retentions::defaults());
    let seconds = [0, 1, 2, 3].map(|n| (1, BASE + n * 1_000, fixed(&format!("670{n}0"))));
    store.observe(&seconds).unwrap();
    let offset = Offset { seq: 4, index: 1 };
    let frozen = store.freeze(offset).unwrap();
    assert!(frozen.pairs.iter().all(|pair| pair.pins.is_empty()), "a row is pinned: {frozen:?}");
    let copy = frozen_path(path.parent().unwrap(), offset);
    let pristine = std::fs::read(&copy).unwrap();
    let second = |n: i64| BASE + n * 1_000;
    let import = |name: &str, alter: &dyn Fn(&Connection)| {
        std::fs::write(&copy, &pristine).unwrap();
        alter(&Connection::open(&copy).unwrap());
        let mut imported = other_key_store(name);
        let result = imported.import_frozen(&copy, &frozen).map_err(|e| format!("{e:#}"));
        (result, imported)
    };
    let refused = async |name: &str, alter: &dyn Fn(&Connection)| {
        let (result, imported) = import(name, alter);
        let error = result.expect_err(name);
        assert!(error.contains("root covers"), "{name}: {error}");
        assert!(every_page(&imported).await.iter().all(|page| page.entries.is_empty()), "{name} left rows");
    };
    let (result, _) = import("marks-frozen-root-pristine", &|_| {});
    assert_eq!(result, Ok(7), "the pristine copy: four seconds and one bucket at each other resolution");

    refused("marks-frozen-root-changed", &|copy| {
        let sql = "UPDATE marks SET close = '67011' WHERE market = 1 AND resolution = 0 AND bucket = ?1";
        assert_eq!(copy.execute(sql, [second(1)]).unwrap(), 1);
    })
    .await;
    refused("marks-frozen-root-swapped", &|copy| {
        let read = "SELECT open, high, low, close FROM marks WHERE market = 1 AND resolution = 0 AND bucket = ?1";
        let prices = |bucket: i64| -> [String; 4] {
            copy.query_row(read, [bucket], |row| Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?])).unwrap()
        };
        let (first, next) = (prices(second(1)), prices(second(2)));
        let write = "UPDATE marks SET open = ?2, high = ?3, low = ?4, close = ?5
             WHERE market = 1 AND resolution = 0 AND bucket = ?1";
        for (bucket, [open, high, low, close]) in [(second(1), next), (second(2), first)] {
            assert_eq!(copy.execute(write, rusqlite::params![bucket, open, high, low, close]).unwrap(), 1);
        }
    })
    .await;
    refused("marks-frozen-root-removed", &|copy| {
        let relink = "UPDATE marks SET prev_bucket = ?1,
             prev_tag = (SELECT tag FROM marks WHERE market = 1 AND resolution = 0 AND bucket = ?1)
             WHERE market = 1 AND resolution = 0 AND bucket = ?2";
        assert_eq!(copy.execute(relink, [second(1), second(3)]).unwrap(), 1);
        let remove = "DELETE FROM marks WHERE market = 1 AND resolution = 0 AND bucket = ?1";
        assert_eq!(copy.execute(remove, [second(2)]).unwrap(), 1);
    })
    .await;
}

/// The freeze reads its copy back under the key that wrote the cache, so a cache whose row the host rewrote gets no
/// root over it, and no copy takes its name.
#[test]
fn a_freeze_refuses_a_row_the_host_rewrote() {
    let (path, mut store) = fresh_store_at("marks-frozen-rewritten", Retentions::defaults());
    fill(&mut store);
    let sql = "UPDATE marks SET close = '1' WHERE market = 2 AND resolution = 1 AND bucket = ?1";
    assert_eq!(Connection::open(&path).unwrap().execute(sql, [BASE]).unwrap(), 1);
    let offset = Offset { seq: 9, index: 1 };
    let error = format!("{:#}", store.freeze(offset).unwrap_err());
    assert!(error.contains("fails authentication"), "{error}");
    assert!(!frozen_path(path.parent().unwrap(), offset).exists(), "the refused copy took its name");
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

/// With the daily snapshots' directory, the copies of the newest `KEPT` snapshots it lists stay however many later
/// freezes no published snapshot names, and so does the newest freeze's, whose snapshot is not listed yet.
#[test]
fn the_copies_of_the_newest_listed_snapshots_are_kept() {
    let (path, mut store) = fresh_store_at("marks-frozen-listed", Retentions::defaults());
    let offsets: Vec<Offset> = (1..=KEPT as u64 + 3).map(|seq| Offset { seq: seq * 10, index: 1 }).collect();
    let snapshots = crate::test_support::scratch_dir("marks-frozen-listed-daily");
    std::fs::create_dir_all(&snapshots).unwrap();
    let listed: Vec<crate::daily_snapshots::Listed> = offsets[..KEPT]
        .iter()
        .map(|offset| crate::daily_snapshots::Listed {
            file: format!("snapshot-{}.json", offset.seq),
            seq: offset.seq,
            index: offset.index,
            ts: 0,
            root: String::new(),
            bytes: 0,
            sha256: String::new(),
        })
        .collect();
    crate::daily_snapshots::write_listing(&snapshots, &listed);
    for (n, offset) in offsets.iter().enumerate() {
        store.observe(&[(1, BASE + n as i64 * 1_000, fixed("67000"))]).unwrap();
        store.freeze_keeping(*offset, Some(&snapshots)).unwrap();
    }
    let dir = path.parent().unwrap();
    let held: Vec<bool> = offsets.iter().map(|offset| frozen_path(dir, *offset).exists()).collect();
    let unpublished = offsets.len() - KEPT - 1;
    assert_eq!(held, [vec![true; KEPT], vec![false; unpublished], vec![true]].concat());
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

/// A store on a fresh scratch file under another marks key than `mac_key`'s, as after a start under keys sealed
/// afresh.
fn other_key_store(name: &str) -> Store {
    Store::open(&crate::test_support::scratch_dir(name).join(MARKS_FILE), Retentions::defaults(), [0x24; 32]).unwrap()
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
