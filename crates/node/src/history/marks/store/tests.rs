//! The marks cache on disk: bucketing, per-resolution retention, seeding that never overwrites
//! log-derived rows, backfill markers, paging, and a rebuild of a file the host altered.

use std::path::PathBuf;

use engine_core::Fixed;
use rusqlite::Connection;

use super::{MARKS_FILE, Reader, SCHEMA, Seed, Store};
use crate::history::marks::{MarkEntry, MarksPage, Provenance, Resolution, Retentions};
use crate::history::store::{read_only, schema_of};
use crate::test_support::scratch_dir;

/// A store on a fresh scratch file.
fn fresh_store(name: &str, retentions: Retentions) -> Store {
    Store::open(&scratch_dir(name).join(MARKS_FILE), retentions).unwrap()
}

/// The store's path on a fresh scratch file.
fn fresh_store_at(name: &str, retentions: Retentions) -> (PathBuf, Store) {
    let path = scratch_dir(name).join(MARKS_FILE);
    (path.clone(), Store::open(&path, retentions).unwrap())
}

/// A one-hour retention for every resolution but 1d, which stays unbounded.
fn hour_retentions() -> Retentions {
    Retentions { s1: 3_600_000, m1: 3_600_000, h1: 3_600_000, d1: None }
}

fn fixed(text: &str) -> Fixed {
    text.parse().unwrap()
}

/// The whole 1m series of `market` as one page.
async fn minute(store: &Store, market: u32) -> MarksPage {
    store.reader().page(market, Resolution::M1, i64::MIN, i64::MAX, 10_000, 10_000).await.unwrap()
}

/// A page entry for comparisons.
fn entry(ts: i64, open: &str, high: &str, low: &str, close: &str, provenance: Provenance) -> MarkEntry {
    MarkEntry { ts, open: fixed(open), high: fixed(high), low: fixed(low), close: fixed(close), provenance }
}

#[tokio::test]
async fn samples_merge_into_ohlc_buckets_at_every_resolution() {
    let mut store = fresh_store("marks-observe", Retentions::defaults());
    let market = 7;
    // Three marks across two seconds and a move up then down.
    store
        .observe(&[
            (market, 1_792_489_875_400, fixed("100")),
            (market, 1_792_489_875_700, fixed("110")),
            (market, 1_792_489_876_200, fixed("90")),
        ])
        .unwrap();
    // Two 1s buckets: 100→110, then 90 alone.
    let page = store.reader().page(market, Resolution::S1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
    assert_eq!(
        page.entries,
        [
            entry(1_792_489_875_000, "100", "110", "100", "110", Provenance::LogDerived),
            entry(1_792_489_876_000, "90", "90", "90", "90", Provenance::LogDerived),
        ]
    );
    // The 1m bucket the samples share covers the whole move; 1h and 1d take it one level up.
    let page = minute(&store, market).await;
    assert_eq!(page.entries, [entry(1_792_489_860_000, "100", "110", "90", "90", Provenance::LogDerived)]);
    for resolution in [Resolution::H1, Resolution::D1] {
        let page = store.reader().page(market, resolution, i64::MIN, i64::MAX, 10, 10).await.unwrap();
        assert_eq!(page.entries.len(), 1, "{resolution:?}");
        assert_eq!(page.entries[0].high, fixed("110"), "{resolution:?}");
    }
    // Another market's rows stay separate.
    store.observe(&[(9, 1_792_489_875_500, fixed("1"))]).unwrap();
    let page = store.reader().page(9, Resolution::S1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
    assert_eq!(page.entries.len(), 1);
}

#[tokio::test]
async fn an_unchanged_mark_keeps_the_bucket_close() {
    let mut store = fresh_store("marks-unchanged", Retentions::defaults());
    let market = 1;
    // The same mark twice within one 1s bucket: no high/low movement, close kept.
    store.observe(&[(market, 1_792_489_875_100, fixed("5")), (market, 1_792_489_875_500, fixed("5"))]).unwrap();
    let page = store.reader().page(market, Resolution::S1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
    assert_eq!(page.entries, [entry(1_792_489_875_000, "5", "5", "5", "5", Provenance::LogDerived)]);
}

#[tokio::test]
async fn retention_prunes_each_resolution_by_its_own_window() {
    let mut store = fresh_store("marks-retention", hour_retentions());
    let market = 1;
    let base = 1_792_000_000_000_i64;
    // One sample an hour and a second before the newest: every bounded resolution prunes it, the
    // 1d bucket at its floor survives.
    store.observe(&[(market, base, fixed("1")), (market, base + 3_600_000 + 1_000, fixed("2"))]).unwrap();
    let older = base + 1_000;
    for resolution in [Resolution::S1, Resolution::M1, Resolution::H1] {
        let page = store.reader().page(market, resolution, i64::MIN, i64::MAX, 10, 10).await.unwrap();
        let kept: Vec<_> = page.entries.iter().map(|entry| entry.ts).collect();
        assert!(kept.iter().all(|&ts| ts > older), "{resolution:?} kept {kept:?}");
        assert_eq!(kept.len(), 1, "{resolution:?} keeps only the newest bucket");
    }
    let page = store.reader().page(market, Resolution::D1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
    assert_eq!(page.entries.len(), 1, "1d is unbounded");
}

#[tokio::test]
async fn seeding_inserts_only_missing_buckets_and_never_overwrites_log_marks() {
    let mut store = fresh_store("marks-seed", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store.observe(&[(market, base + 10_000, fixed("100"))]).unwrap();
    // A provider candle over the log-derived minute, and one for a minute the log never saw.
    let inserted = store
        .seed(
            &[
                Seed {
                    market,
                    resolution: Resolution::M1,
                    ts: base,
                    open: fixed("1"),
                    high: fixed("2"),
                    low: fixed("0"),
                    close: fixed("1"),
                },
                Seed {
                    market,
                    resolution: Resolution::M1,
                    ts: base + 60_000,
                    open: fixed("3"),
                    high: fixed("4"),
                    low: fixed("2"),
                    close: fixed("3"),
                },
            ],
            base + 60_000,
        )
        .unwrap();
    assert_eq!(inserted, 1, "the log-derived bucket is kept, the new one inserted");
    let page = minute(&store, market).await;
    assert_eq!(page.entries.len(), 2);
    // The log-derived bucket is untouched: still the log mark, still log-derived.
    assert_eq!(page.entries[0], entry(base, "100", "100", "100", "100", Provenance::LogDerived));
    assert_eq!(page.entries[1], entry(base + 60_000, "3", "4", "2", "3", Provenance::SeededFromProvider));
    // A live sample after the seed takes the seeded bucket over to log-derived.
    store.observe(&[(market, base + 60_000 + 1_000, fixed("3"))]).unwrap();
    let page = minute(&store, market).await;
    assert_eq!(page.entries[1], entry(base + 60_000, "3", "4", "2", "3", Provenance::LogDerived));
}

#[tokio::test]
async fn a_backfilled_marker_survives_a_reopen() {
    let (path, mut store) = fresh_store_at("marks-markers", Retentions::defaults());
    store.mark_backfilled(3, "coinbase", "BTC-USD", 1_792_000_000_000).unwrap();
    assert_eq!(store.backfilled().unwrap(), [3].into_iter().collect());
    drop(store);
    let store = Store::open(&path, Retentions::defaults()).unwrap();
    assert_eq!(store.backfilled().unwrap(), [3].into_iter().collect());
}

#[tokio::test]
async fn pages_are_oldest_first_capped_with_a_next_cursor() {
    let (path, mut store) = fresh_store_at("marks-pages", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    for n in 0..5 {
        store.observe(&[(market, base + n * 60_000, Fixed::from_int(i64::from(n)))]).unwrap();
    }
    drop(store);
    let reader = Reader::new(path);
    // A window larger than the cap pages by following `next` as `from_ms`.
    let mut from = i64::MIN;
    let mut seen = Vec::new();
    loop {
        let page = reader.page(market, Resolution::M1, from, i64::MAX, 2, 2).await.unwrap();
        assert!(page.entries.len() <= 2, "{page:?}");
        assert_eq!(page.max_limit, 2);
        seen.extend(page.entries.iter().map(|entry| entry.ts));
        match page.next {
            Some(next) => from = next,
            None => break,
        }
    }
    assert_eq!(seen, (0..5).map(|n| base + n * 60_000).collect::<Vec<_>>());
    // `to_ms` bounds the window from above, inclusively.
    let page = reader.page(market, Resolution::M1, base + 60_000, base + 3 * 60_000, 10, 10).await.unwrap();
    assert_eq!(
        page.entries.iter().map(|entry| entry.ts).collect::<Vec<_>>(),
        vec![base + 60_000, base + 2 * 60_000, base + 3 * 60_000]
    );
    assert_eq!(page.next, None);
}

#[tokio::test]
async fn a_file_the_host_altered_is_rebuilt_not_served() {
    let (path, mut store) = fresh_store_at("marks-rebuild", Retentions::defaults());
    store.observe(&[(1, 1_792_489_860_000, fixed("1"))]).unwrap();
    drop(store);
    // The host drops a table: the next open discards the file and rebuilds it empty.
    Connection::open(&path).unwrap().execute_batch("DROP TABLE backfilled").unwrap();
    let store = Store::open(&path, Retentions::defaults()).unwrap();
    assert!(store.backfilled().unwrap().is_empty());
    let page = store.reader().page(1, Resolution::S1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
    assert!(page.entries.is_empty(), "the rebuilt cache starts empty");
}

#[tokio::test]
async fn a_schema_this_code_did_not_create_is_refused_by_every_read_and_write() {
    let (path, mut store) = fresh_store_at("marks-schema", Retentions::defaults());
    store.observe(&[(1, 1_792_489_860_000, fixed("1"))]).unwrap();
    // The host adds a trigger while the writer holds the file open.
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TRIGGER wipe AFTER INSERT ON marks BEGIN DELETE FROM marks; END")
        .unwrap();
    let reader = Reader::new(path.clone());
    let err = reader.page(1, Resolution::S1, i64::MIN, i64::MAX, 10, 10).await.unwrap_err();
    assert!(format!("{err:#}").contains("did not create"), "{err:#}");
    drop(reader);
    // The writer's next write refuses too, and changes nothing; the host's trigger never runs.
    let err = store.observe(&[(1, 1_792_489_870_000, fixed("2"))]).err().expect("refused");
    assert!(format!("{err:#}").contains("did not create"), "{err:#}");
    let rows: i64 = store.conn.query_row("SELECT count(*) FROM marks", [], |row| row.get(0)).unwrap();
    assert_eq!(rows, 4, "one sample wrote four resolution buckets, and the refused write added none");
}

#[tokio::test]
async fn every_connection_to_the_cache_runs_defensive_with_an_untrusted_schema() {
    let (path, store) = fresh_store_at("marks-defensive", Retentions::defaults());
    let reader = read_only(&path).unwrap();
    for (side, conn) in [("writer", &store.conn), ("reader", &reader)] {
        use rusqlite::config::DbConfig;
        assert!(conn.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE).unwrap(), "{side}");
        assert!(!conn.db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER).unwrap(), "{side}");
        assert!(!conn.db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW).unwrap(), "{side}");
        assert!(!conn.db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA).unwrap(), "{side}");
    }
}

// The schema builds and lists what `open` expects.
#[tokio::test]
async fn the_schema_is_what_open_expects() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    assert!(!schema_of(&conn).unwrap().is_empty());
}
