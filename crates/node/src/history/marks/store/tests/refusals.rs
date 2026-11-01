//! Chain refusals: the writer and the recovery reset declining to heal a host deletion,
//! whatever shape the hole takes — a recreated bucket, a batch that fills above it, a prune
//! that could re-floor over it, or a reset row that records it.

use rusqlite::Connection;

use super::*;

#[tokio::test]
async fn a_seed_batch_recreating_a_deleted_bucket_and_filling_above_it_refuses() {
    let (path, mut store) = fresh_store_at("marks-chain-seed-batch-refill", Retentions::defaults());
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
    // The host deletes B. A backfill batch that recreates B and also fills X between B and C
    // would let X's re-link take the gap through the recreated B: the batch refuses, because the
    // row above can only name a bucket that predates the batch.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base + 60_000])
        .unwrap();
    let err = store
        .seed(
            &[
                Seed {
                    market,
                    resolution: Resolution::M1,
                    ts: base + 60_000,
                    open: fixed("9"),
                    high: fixed("9"),
                    low: fixed("9"),
                    close: fixed("9"),
                },
                Seed {
                    market,
                    resolution: Resolution::M1,
                    ts: base + 90_000,
                    open: fixed("8"),
                    high: fixed("8"),
                    low: fixed("8"),
                    close: fixed("8"),
                },
            ],
            base + 180_000,
        )
        .expect_err("healed a deletion through a batch");
    assert!(format!("{err:#}").contains("does not name the bucket below it"), "{err:#}");
}

#[tokio::test]
async fn a_reset_refuses_to_launder_a_deletion_through_a_row_it_removes() {
    let (path, mut store) = fresh_store_at("marks-chain-reset-launder", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    // A log-derived, B seeded, C log-derived, D seeded: B is the interior seeded bucket.
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
    store.observe(&[(market, base + 120_000, fixed("3"))]).unwrap();
    store
        .seed(
            &[Seed {
                market,
                resolution: Resolution::M1,
                ts: base + 180_000,
                open: fixed("4"),
                high: fixed("4"),
                low: fixed("4"),
                close: fixed("4"),
            }],
            base + 180_000,
        )
        .unwrap();
    // The host deletes the interior seeded bucket B. The reset will not re-chain D over the
    // hole — C's own link names the missing B — and rebuilds the derived cache from nothing
    // instead, so the deletion cannot be laundered into a servable chain.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base + 60_000])
        .unwrap();
    store.reset_log_derived().unwrap();
    let page = minute(&store, market).await;
    assert!(page.entries.is_empty(), "the refused reset rebuilt the cache empty");
}

#[tokio::test]
async fn a_prune_refuses_to_erase_the_row_recording_a_host_deletion() {
    let (path, mut store) = fresh_store_at("marks-chain-prune-launder", hour_retentions());
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
    // The host deletes the interior bucket B. A flush whose horizon sweeps past C deletes the
    // row that still records the hole: the prune refuses, because the expiring prefix's own
    // links must be intact before the delete erases them.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base + 60_000])
        .unwrap();
    let err =
        store.observe(&[(market, base + 3_780_000, fixed("5"))]).expect_err("erased the record of a host deletion");
    assert!(format!("{err:#}").contains("already missing"), "{err:#}");
}

#[tokio::test]
async fn a_malformed_end_tag_fails_the_read_instead_of_aborting() {
    let (path, mut store) = fresh_store_at("marks-chain-malformed-tag", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    store.observe(&[(market, base, fixed("1"))]).unwrap();
    // The host truncates the head row's tag to one byte; the read must fail like any other
    // corruption, never panic (release builds abort on panic).
    Connection::open(&path)
        .unwrap()
        .execute("UPDATE marks SET tag = x'01' WHERE market = 1 AND resolution = 1", [])
        .unwrap();
    let err = store
        .reader()
        .page(market, Resolution::M1, i64::MIN, i64::MAX, 10, 10)
        .await
        .expect_err("served a malformed tag");
    assert!(format!("{err:#}").contains("chain ends"), "{err:#}");
}

#[tokio::test]
async fn a_prune_refuses_an_edited_link_even_below_the_horizon() {
    let (path, mut store) = fresh_store_at("marks-chain-prune-edited", hour_retentions());
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
    // The host deletes B and edits C's link to skip the hole, without the tag that binds it:
    // the row no longer authenticates, and the prune refuses to delete it.
    let host = Connection::open(&path).unwrap();
    host.execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base + 60_000]).unwrap();
    host.execute(
        "UPDATE marks SET prev_bucket = ?1 WHERE market = 1 AND resolution = 1 AND bucket = ?2",
        [base, base + 120_000],
    )
    .unwrap();
    drop(host);
    let err = store.observe(&[(market, base + 3_780_000, fixed("5"))]).expect_err("erased an edited row's record");
    assert!(format!("{err:#}").contains("fails authentication"), "{err:#}");
}

#[tokio::test]
async fn a_backfill_refuses_to_relink_a_restored_floor_row() {
    let (path, mut store) = fresh_store_at("marks-chain-restored-floor", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    // C is the floor beneath D; capture the authentic NULL-link version of its row.
    store.observe(&[(market, base + 120_000, fixed("3")), (market, base + 180_000, fixed("4"))]).unwrap();
    let old_tag: Vec<u8> = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT tag FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1",
            [base + 120_000],
            |row| row.get(0),
        )
        .unwrap();
    // A backfill seeds A and B below C: C re-links over them, floor and head anchors move down.
    store
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
            base + 180_000,
        )
        .unwrap();
    // The host deletes B and restores C's captured floor row, tag and all: the ends still match
    // the committed anchors, but reads refuse the broken chain. A backfill retry recreating B
    // must not re-link the restored row over the hole.
    let host = Connection::open(&path).unwrap();
    host.execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base + 60_000]).unwrap();
    host.execute(
        "UPDATE marks SET prev_bucket = NULL, tag = ?1 WHERE market = 1 AND resolution = 1 AND bucket = ?2",
        rusqlite::params![old_tag, base + 120_000],
    )
    .unwrap();
    drop(host);
    let page = store.reader().page(market, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await;
    assert!(page.is_err(), "the restored floor row refuses reads");
    let err = store
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
            base + 180_000,
        )
        .expect_err("re-linked a restored floor row");
    assert!(format!("{err:#}").contains("does not name the bucket below it"), "{err:#}");
}

#[tokio::test]
async fn a_seed_batch_relinking_past_a_surviving_older_row_refuses() {
    let (path, mut store) = fresh_store_at("marks-chain-run-below-survivor", Retentions::defaults());
    let market = 1;
    let base = 1_792_489_860_000;
    // C is the floor beneath D; capture its authentic NULL-link row.
    store.observe(&[(market, base + 120_000, fixed("3")), (market, base + 180_000, fixed("4"))]).unwrap();
    let old_tag: Vec<u8> = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT tag FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1",
            [base + 120_000],
            |row| row.get(0),
        )
        .unwrap();
    // A backfill seeds A, B1, B2 below C.
    store
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
                    ts: base + 30_000,
                    open: fixed("2"),
                    high: fixed("2"),
                    low: fixed("2"),
                    close: fixed("2"),
                },
                Seed {
                    market,
                    resolution: Resolution::M1,
                    ts: base + 60_000,
                    open: fixed("9"),
                    high: fixed("9"),
                    low: fixed("9"),
                    close: fixed("9"),
                },
            ],
            base + 180_000,
        )
        .unwrap();
    // The host deletes B1 and B2 and restores C's captured floor row; the pre-existing A
    // survives below. A backfill retry recreating both must not re-link C over A's head.
    let host = Connection::open(&path).unwrap();
    host.execute(
        "DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket IN (?1, ?2)",
        [base + 30_000, base + 60_000],
    )
    .unwrap();
    host.execute(
        "UPDATE marks SET prev_bucket = NULL, tag = ?1 WHERE market = 1 AND resolution = 1 AND bucket = ?2",
        rusqlite::params![old_tag, base + 120_000],
    )
    .unwrap();
    drop(host);
    let err = store
        .seed(
            &[
                Seed {
                    market,
                    resolution: Resolution::M1,
                    ts: base + 30_000,
                    open: fixed("2"),
                    high: fixed("2"),
                    low: fixed("2"),
                    close: fixed("2"),
                },
                Seed {
                    market,
                    resolution: Resolution::M1,
                    ts: base + 60_000,
                    open: fixed("9"),
                    high: fixed("9"),
                    low: fixed("9"),
                    close: fixed("9"),
                },
            ],
            base + 180_000,
        )
        .expect_err("re-linked over a surviving older row");
    assert!(format!("{err:#}").contains("does not name the bucket below it"), "{err:#}");
}

#[tokio::test]
async fn a_prune_refuses_to_publish_a_restored_row_as_the_new_floor() {
    let (path, mut store) = fresh_store_at("marks-chain-prune-restored-floor", hour_retentions());
    let market = 1;
    let base = 1_792_489_860_000;
    // C is the floor beneath D; capture its authentic NULL-link row, then seed A and B below it.
    store.observe(&[(market, base + 120_000, fixed("3")), (market, base + 180_000, fixed("4"))]).unwrap();
    let old_tag: Vec<u8> = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT tag FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1",
            [base + 120_000],
            |row| row.get(0),
        )
        .unwrap();
    store
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
            base + 180_000,
        )
        .unwrap();
    // The host deletes B and restores C's captured floor row. A flush whose horizon sweeps A and
    // the hole past must not publish the restored row as the new floor.
    let host = Connection::open(&path).unwrap();
    host.execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base + 60_000]).unwrap();
    host.execute(
        "UPDATE marks SET prev_bucket = NULL, tag = ?1 WHERE market = 1 AND resolution = 1 AND bucket = ?2",
        rusqlite::params![old_tag, base + 120_000],
    )
    .unwrap();
    drop(host);
    let err =
        store.observe(&[(market, base + 3_720_000, fixed("5"))]).expect_err("published a restored row as the floor");
    assert!(format!("{err:#}").contains("names nothing"), "{err:#}");
}

#[tokio::test]
async fn a_prune_refuses_a_floor_that_skips_the_last_deleted_bucket() {
    let (path, mut store) = fresh_store_at("marks-chain-prune-skip-last", hour_retentions());
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
    // The host deletes Z (between the oldest Y and A), leaving A naming it. A flush whose
    // horizon sweeps only Y must not re-floor A: its authentic link names Z, which this prune
    // did not delete.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base + 60_000])
        .unwrap();
    let err =
        store.observe(&[(market, base + 3_660_000, fixed("5"))]).expect_err("re-floored over a hole below the horizon");
    assert!(format!("{err:#}").contains("not the last this prune deleted"), "{err:#}");
}

#[tokio::test]
async fn a_prune_refuses_a_floor_linking_an_earlier_deleted_bucket() {
    let (path, mut store) = fresh_store_at("marks-chain-prune-earlier-link", hour_retentions());
    let market = 1;
    let base = 1_792_489_860_000;
    // A and C first: C's authentic row links to A. Then B between them: C re-links to B.
    store.observe(&[(market, base, fixed("1")), (market, base + 120_000, fixed("3"))]).unwrap();
    let old_tag: Vec<u8> = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT tag FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1",
            [base + 120_000],
            |row| row.get(0),
        )
        .unwrap();
    store.observe(&[(market, base + 180_000, fixed("4"))]).unwrap();
    store.observe(&[(market, base + 60_000, fixed("2"))]).unwrap();
    // The host restores C's captured A-link row while B survives. A flush whose horizon sweeps
    // A and B must refuse: C's authentic link names an earlier bucket than the last deleted.
    let host = Connection::open(&path).unwrap();
    host.execute(
        "UPDATE marks SET prev_bucket = ?1, tag = ?2 WHERE market = 1 AND resolution = 1 AND bucket = ?3",
        rusqlite::params![base, old_tag, base + 120_000],
    )
    .unwrap();
    drop(host);
    let err = store
        .observe(&[(market, base + 3_720_000, fixed("5"))])
        .expect_err("published a floor linking an earlier bucket");
    assert!(format!("{err:#}").contains("not the last this prune deleted"), "{err:#}");
}
