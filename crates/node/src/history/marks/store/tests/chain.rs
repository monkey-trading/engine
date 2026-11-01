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
    assert!(format!("{err:#}").contains("no committed chain anchor"), "{err:#}");
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
    // The host deletes the bucket below the newest row; the next write refuses before touching
    // the chain: the pair's committed floor no longer matches. The row it would have merged stays
    // as it was, and reads keep refusing against the committed anchor.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base])
        .unwrap();
    let err = store.observe(&[(market, base + 120_000, fixed("4"))]).expect_err("re-linked");
    assert!(format!("{err:#}").contains("no longer matches its committed anchor"), "{err:#}");
    assert_eq!(stored_close(&store, market, Resolution::M1, base + 120_000), "3");
    let err =
        store.reader().page(market, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await.expect_err("served over a hole");
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
}

#[tokio::test]
async fn a_refused_reset_rebuilds_the_cache_from_nothing() {
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
    // The host deletes the log-derived floor the seed links to, mid-run: the reset refuses to
    // re-link over the hole, and the derived cache is rebuilt from nothing — the log re-derives
    // it, a configured backfill re-seeds it.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base])
        .unwrap();
    store.reset_log_derived().unwrap();
    let page = minute(&store, market).await;
    assert!(page.entries.is_empty(), "the refused reset rebuilt the cache empty");
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

#[tokio::test]
async fn pagination_over_a_bounded_window_serves_every_page() {
    let (_path, mut store) = fresh_store_at("marks-chain-paged-window", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store
        .observe(&[
            (market, base, fixed("1")),
            (market, base + 60_000, fixed("2")),
            (market, base + 120_000, fixed("3")),
            (market, base + 180_000, fixed("4")),
        ])
        .unwrap();
    // A window that truncates at `limit` mid-way, with rows above `to`: the page closes against
    // its own last row and the next page, not against the row above the whole window.
    let reader = store.reader();
    let mut from = base;
    let mut seen = Vec::new();
    loop {
        let page = reader.page(market, Resolution::M1, from, base + 120_000, 2, 2).await.unwrap();
        seen.extend(page.entries.iter().map(|entry| entry.ts));
        match page.next {
            Some(next) => from = next,
            None => break,
        }
    }
    assert_eq!(seen, (0..3).map(|n| base + n * 60_000).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_seed_into_a_bucket_a_successor_still_names_refuses() {
    let (path, mut store) = fresh_store_at("marks-chain-seed-refill", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store
        .observe(&[
            (market, base, fixed("1")),
            (market, base + 60_000, fixed("2")),
            (market, base + 120_000, fixed("3")),
        ])
        .unwrap();
    // The host deletes the middle bucket; a backfill that recreates it would take the link the
    // row above still names — the seed refuses, never healing the deletion with provider prices.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base + 60_000])
        .unwrap();
    let err = store
        .seed(
            &[Seed {
                market,
                resolution: Resolution::M1,
                ts: base + 60_000,
                open: fixed("9"),
                high: fixed("9"),
                low: fixed("9"),
                close: fixed("9"),
            }],
            base + 120_000,
        )
        .expect_err("re-filled a host deletion");
    assert!(format!("{err:#}").contains("above the insert"), "{err:#}");
    // A live mark into the same bucket refuses the same way.
    let err = store.observe(&[(market, base + 60_000, fixed("9"))]).expect_err("re-filled");
    assert!(format!("{err:#}").contains("above the insert"), "{err:#}");
}

#[tokio::test]
async fn a_prune_leaves_an_unrelated_pairs_dangling_floor_for_its_readers_to_refuse() {
    let (path, mut store) = fresh_store_at("marks-chain-prune-refuse", hour_retentions());
    let base = 1_792_489_860_000;
    store.observe(&[(1, base, fixed("1")), (1, base + 60_000, fixed("2"))]).unwrap();
    // The host deletes market 1's floor, leaving its head naming the hole, inside the retention
    // window. A prune that only touches market 2 neither heals nor adopts market 1: its next
    // read refuses the broken chain, and its next write refuses too.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base])
        .unwrap();
    store.observe(&[(2, base - 2 * 3_600_000, fixed("0")), (2, base, fixed("3"))]).unwrap();
    let err = store.reader().page(1, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await.expect_err("served over a hole");
    assert!(format!("{err:#}").contains("rolled back") || format!("{err:#}").contains("breaks the chain"), "{err:#}");
    let err = store.observe(&[(1, base + 60_000, fixed("9"))]).expect_err("merged over a hole");
    assert!(
        format!("{err:#}").contains("no longer below it")
            || format!("{err:#}").contains("no longer matches its committed anchor"),
        "{err:#}"
    );
}

#[tokio::test]
async fn an_unrelated_write_keeps_the_committed_anchor_and_reads_refuse() {
    let (path, mut store) = fresh_store_at("marks-chain-unrelated-write", Retentions::defaults());
    let base = 1_792_489_860_000;
    store.observe(&[(1, base, fixed("1")), (1, base + 60_000, fixed("2"))]).unwrap();
    store.observe(&[(2, base, fixed("5"))]).unwrap();
    // The host deletes market 1's head; a write to market 2 commits untouched pairs' anchors
    // unchanged, so market 1's truncated series fails its next read.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base + 60_000])
        .unwrap();
    store.observe(&[(2, base + 60_000, fixed("6"))]).unwrap();
    let err = store
        .reader()
        .page(1, Resolution::M1, i64::MIN, i64::MAX, 10, 10)
        .await
        .expect_err("served a truncated series");
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    let page = store.reader().page(2, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
    assert_eq!(page.entries.len(), 2);
}

#[tokio::test]
async fn a_restored_prefix_snapshot_fails_the_committed_floor() {
    let (path, mut store) = fresh_store_at("marks-chain-floor-rollback", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store.observe(&[(market, base, fixed("1")), (market, base + 60_000, fixed("2"))]).unwrap();
    store.checkpoint().unwrap();
    let snapshot = std::fs::read(&path).unwrap();
    // A backfill seeds below the floor: the floor moves down and the row above re-links.
    store
        .seed(
            &[Seed {
                market,
                resolution: Resolution::M1,
                ts: base - 60_000,
                open: fixed("0"),
                high: fixed("0"),
                low: fixed("0"),
                close: fixed("0"),
            }],
            base + 60_000,
        )
        .unwrap();
    // The host restores the pre-backfill file: the committed floor names the seeded bucket the
    // snapshot no longer holds, and the read refuses.
    store.checkpoint().unwrap();
    std::fs::write(&path, &snapshot).unwrap();
    for suffix in ["-wal", "-shm"] {
        let mut side = path.clone().into_os_string();
        side.push(suffix);
        let _ = std::fs::remove_file(side);
    }
    let err = store
        .reader()
        .page(market, Resolution::M1, i64::MIN, i64::MAX, 10, 10)
        .await
        .expect_err("served a prefix rollback");
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
}

#[tokio::test]
async fn a_raced_commit_re_synchronizes_before_a_mismatch_is_a_rollback() {
    use std::sync::{Arc, Mutex};

    let (_path, mut store) = fresh_store_at("marks-chain-stale-retry", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store.observe(&[(market, base, fixed("1"))]).unwrap();
    // An anchor one commit newer than the file — the shape a racing flush leaves. The first
    // attempts report the stale-anchor race; the last, re-synchronized, reports the rollback.
    let mut anchors: super::super::ChainAnchors = Default::default();
    anchors.insert(
        (market, Resolution::M1),
        super::super::ChainAnchor {
            floor: super::super::ChainHead { bucket: base + 60_000, tag: [7; 16] },
            head: super::super::ChainHead { bucket: base + 60_000, tag: [7; 16] },
        },
    );
    let reader = super::super::Reader::new(
        store.path().to_owned(),
        hmac::Hmac::new_from_slice(&mac_key()).unwrap(),
        Arc::new(Mutex::new(anchors.clone())),
    );
    let err = reader
        .page(market, Resolution::M1, i64::MIN, i64::MAX, 10, 10)
        .await
        .expect_err("served against a fabricated anchor");
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    // The stale attempt alone names the race, as a typed error, not a rollback.
    let query = super::super::PageQuery {
        market,
        resolution: Resolution::M1,
        from: i64::MIN,
        to: i64::MAX,
        limit: 10,
        max_limit: 10,
    };
    let err = super::super::reader::read_page_once(
        store.path(),
        &super::expected_schema(),
        &store.mac,
        &Arc::new(Mutex::new(anchors)),
        &query,
        true,
    )
    .expect_err("no race reported");
    assert!(err.downcast_ref::<super::super::reader::StaleAnchor>().is_some(), "{err:#}");
}
