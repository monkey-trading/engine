//! The chained index over the marks cache: every row names the bucket below it and every read
//! checks those links and the committed chain heads, so a host that deletes, reorders, or rolls
//! the cache back is refused at serve time, mid-run, and the writer refuses, never re-links, over
//! a predecessor that vanished outside its own transaction.

use hmac::digest::KeyInit as _;
use rusqlite::Connection;

use super::*;

/// The stored chain link of one row, as the host would read it.
fn stored_prev(store: &Store, market: u32, resolution: Resolution, bucket: i64) -> Option<i64> {
    store
        .conn
        .query_row(
            "SELECT prev_bucket FROM marks WHERE market = ?1 AND resolution = ?2 AND bucket = ?3",
            [i64::from(market), resolution as i64, bucket],
            |row| row.get(0),
        )
        .unwrap()
}

/// The stored close of one row, as the host would read it.
fn stored_close(store: &Store, market: u32, resolution: Resolution, bucket: i64) -> String {
    store
        .conn
        .query_row(
            "SELECT close FROM marks WHERE market = ?1 AND resolution = ?2 AND bucket = ?3",
            [i64::from(market), resolution as i64, bucket],
            |row| row.get(0),
        )
        .unwrap()
}

#[tokio::test]
async fn every_row_names_the_bucket_below_it_and_the_floor_names_nothing() {
    let (path, mut store) = fresh_store_at("marks-chain-links", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store
        .observe(&[
            (market, base, fixed("1")),
            (market, base + 60_000, fixed("2")),
            (market, base + 120_000, fixed("3")),
        ])
        .unwrap();
    assert_eq!(stored_prev(&store, market, Resolution::M1, base), None, "the floor chains from nothing");
    assert_eq!(stored_prev(&store, market, Resolution::M1, base + 60_000), Some(base));
    assert_eq!(stored_prev(&store, market, Resolution::M1, base + 120_000), Some(base + 60_000));
    // The links are readable state: a reopened store serves the chain it committed.
    drop(store);
    let store = Store::open(&path, Retentions::defaults(), mac_key()).unwrap();
    let page = minute(&store, market).await;
    assert_eq!(page.entries.len(), 3);
}

#[tokio::test]
async fn a_deleted_bucket_breaks_the_chain_at_serve_time() {
    let (path, mut store) = fresh_store_at("marks-chain-delete", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store
        .observe(&[
            (market, base, fixed("1")),
            (market, base + 60_000, fixed("2")),
            (market, base + 120_000, fixed("3")),
        ])
        .unwrap();
    // The host deletes a bucket while the node runs; every page spanning the hole refuses.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base + 60_000])
        .unwrap();
    let reader = store.reader();
    for (from, to) in [(i64::MIN, i64::MAX), (base + 60_000, i64::MAX), (i64::MIN, base + 60_000)] {
        let err = reader.page(market, Resolution::M1, from, to, 10, 10).await.expect_err("served over a hole");
        assert!(format!("{err:#}").contains("breaks the chain"), "{err:#}");
    }
}

#[tokio::test]
async fn a_restored_snapshot_fails_the_committed_chain_head() {
    let (path, mut store) = fresh_store_at("marks-chain-rollback", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store.observe(&[(market, base, fixed("1"))]).unwrap();
    store.checkpoint().unwrap();
    let snapshot = std::fs::read(&path).unwrap();
    store.observe(&[(market, base + 60_000, fixed("2"))]).unwrap();
    // The host restores the old file, dropping the newest bucket: the committed head no longer
    // matches, and even an empty window over the pair refuses before anything is served.
    store.checkpoint().unwrap();
    std::fs::write(&path, &snapshot).unwrap();
    for suffix in ["-wal", "-shm"] {
        let mut side = path.clone().into_os_string();
        side.push(suffix);
        let _ = std::fs::remove_file(side);
    }
    for (from, to) in [(base + 120_000, i64::MAX), (i64::MIN, i64::MAX)] {
        let err = store.reader().page(market, Resolution::M1, from, to, 10, 10).await.expect_err("served a rollback");
        assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    }
}

#[tokio::test]
async fn rows_no_committed_head_names_are_refused() {
    let (_path, mut store) = fresh_store_at("marks-chain-unanchored", Retentions::defaults());
    store.observe(&[(1, 1_792_489_860_000, fixed("1"))]).unwrap();
    // A reader whose anchors were never committed — the state a host-written or rolled-forward
    // file would face — refuses rows no chain head names.
    let reader = super::super::Reader::new(
        store.path().to_owned(),
        hmac::Hmac::new_from_slice(&mac_key()).unwrap(),
        Default::default(),
    );
    let err = reader.page(1, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await.expect_err("served");
    assert!(format!("{err:#}").contains("no committed chain head"), "{err:#}");
}

#[tokio::test]
async fn an_unreached_market_still_answers_an_empty_page() {
    let store = fresh_store("marks-chain-empty", Retentions::defaults());
    let page = minute(&store, 99).await;
    assert!(page.entries.is_empty());
}

#[tokio::test]
async fn seeding_below_the_floor_re_links_the_row_above() {
    let mut store = fresh_store("marks-chain-seed-floor", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store.observe(&[(market, base + 120_000, fixed("3"))]).unwrap();
    // The seed lands below the floor and in the gap the log left: both rows above re-link.
    let inserted = store
        .seed(
            &[
                Seed {
                    market,
                    resolution: Resolution::M1,
                    ts: base,
                    open: fixed("1"),
                    high: fixed("1"),
                    low: fixed("1"),
                    close: fixed("1"),
                },
                Seed {
                    market,
                    resolution: Resolution::M1,
                    ts: base + 60_000,
                    open: fixed("2"),
                    high: fixed("2"),
                    low: fixed("2"),
                    close: fixed("2"),
                },
            ],
            base + 120_000,
        )
        .unwrap();
    assert_eq!(inserted, 2);
    assert_eq!(stored_prev(&store, market, Resolution::M1, base), None);
    assert_eq!(stored_prev(&store, market, Resolution::M1, base + 60_000), Some(base));
    assert_eq!(stored_prev(&store, market, Resolution::M1, base + 120_000), Some(base + 60_000));
    let page = minute(&store, market).await;
    assert_eq!(page.entries.len(), 3);
}

#[tokio::test]
async fn a_live_mark_filling_a_gap_re_links_the_row_above() {
    let mut store = fresh_store("marks-chain-gap-fill", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store.observe(&[(market, base, fixed("1"))]).unwrap();
    store.observe(&[(market, base + 120_000, fixed("3"))]).unwrap();
    assert_eq!(stored_prev(&store, market, Resolution::M1, base + 120_000), Some(base));
    store.observe(&[(market, base + 60_000, fixed("2"))]).unwrap();
    assert_eq!(stored_prev(&store, market, Resolution::M1, base + 120_000), Some(base + 60_000));
    let page = minute(&store, market).await;
    assert_eq!(page.entries.len(), 3);
}

#[tokio::test]
async fn pruning_re_floors_the_pair_it_cut() {
    let mut store = fresh_store("marks-chain-prune", hour_retentions());
    let market = 1;
    let base = 1_792_000_000_000_i64;
    store.observe(&[(market, base, fixed("1")), (market, base + 3_600_000 + 1_000, fixed("2"))]).unwrap();
    let floor = Resolution::M1.bucket(base + 3_600_000 + 1_000);
    assert_eq!(stored_prev(&store, market, Resolution::M1, floor), None, "the pruned pair re-floors");
    let page = store.reader().page(market, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
    assert_eq!(page.entries.len(), 1);
}

#[tokio::test]
async fn a_successor_link_naming_a_deleted_bucket_refuses_the_next_write() {
    let (path, mut store) = fresh_store_at("marks-chain-write-refused", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store.observe(&[(market, base, fixed("1")), (market, base + 120_000, fixed("3"))]).unwrap();
    // The host deletes the bucket below the newest row; the next write into that row refuses to
    // re-link over the hole instead of healing it, and the row it would have merged stays as it
    // was — while reads keep refusing the broken chain.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base])
        .unwrap();
    let err = store.observe(&[(market, base + 120_000, fixed("4"))]).expect_err("re-linked");
    assert!(format!("{err:#}").contains("no longer below it"), "{err:#}");
    assert_eq!(stored_close(&store, market, Resolution::M1, base + 120_000), "3");
    // The page over the pair refuses too: the row above the hole still names the deleted bucket.
    let err =
        store.reader().page(market, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await.expect_err("served over a hole");
    assert!(format!("{err:#}").contains("breaks the chain"), "{err:#}");
}

#[tokio::test]
async fn reset_re_links_only_over_what_it_deleted() {
    let (path, mut store) = fresh_store_at("marks-chain-reset", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store.observe(&[(market, base, fixed("1"))]).unwrap();
    store
        .seed(
            &[Seed {
                market,
                resolution: Resolution::M1,
                ts: base + 60_000,
                open: fixed("2"),
                high: fixed("2"),
                low: fixed("2"),
                close: fixed("2"),
            }],
            base + 60_000,
        )
        .unwrap();
    // The host deletes the log-derived floor the seed links to. The reset's own delete removes
    // nothing (the row is already gone), so the survivor's link names a bucket no delete removed
    // and the reset refuses, rather than re-linking over the host's deletion.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base])
        .unwrap();
    let err = store.reset_log_derived().expect_err("re-linked over a host deletion");
    assert!(format!("{err:#}").contains("no delete removed"), "{err:#}");
}

#[tokio::test]
async fn a_reset_deleting_the_link_re_links_the_survivor() {
    let mut store = fresh_store("marks-chain-reset-relink", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store.observe(&[(market, base, fixed("1"))]).unwrap();
    store
        .seed(
            &[Seed {
                market,
                resolution: Resolution::M1,
                ts: base + 60_000,
                open: fixed("2"),
                high: fixed("2"),
                low: fixed("2"),
                close: fixed("2"),
            }],
            base + 60_000,
        )
        .unwrap();
    // The reset deletes the log-derived bucket the seed linked to: the survivor re-floors, and
    // the re-derivation that follows inserts over it cleanly.
    store.reset_log_derived().unwrap();
    assert_eq!(stored_prev(&store, market, Resolution::M1, base + 60_000), None);
    store.observe(&[(market, base, fixed("1"))]).unwrap();
    assert_eq!(stored_prev(&store, market, Resolution::M1, base + 60_000), Some(base));
    let page = minute(&store, market).await;
    assert_eq!(page.entries.len(), 2);
}
