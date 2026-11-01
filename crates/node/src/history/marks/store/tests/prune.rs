//! Prune validation: which pairs a write must validate before its prune deletes, and the
//! refusals when the file names pairs the write never checked.

use rusqlite::Connection;

use super::*;

#[tokio::test]
async fn a_prune_of_an_unsampled_pair_publishes_its_new_floor() {
    let (_path, mut store) = fresh_store_at("marks-chain-unsampled-prune", hour_retentions());
    let base = 1_792_489_860_000;
    // Market 1's floor is an hour behind market 2's newest: market 2's flush prunes market 1.
    store.observe(&[(1, base, fixed("1")), (1, base + 3_600_000, fixed("2"))]).unwrap();
    store.observe(&[(2, base + 3_600_000 + 60_000, fixed("5"))]).unwrap();
    store.observe(&[(2, base + 2 * 3_600_000, fixed("6"))]).unwrap();
    let page = store.reader().page(1, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
    assert_eq!(page.entries.len(), 1, "market 1's expired floor pruned");
    assert_eq!(page.entries[0].ts, base + 3_600_000);
    // The pruned pair still reads and writes against its committed anchor.
    store.observe(&[(1, base + 3_600_000, fixed("9"))]).unwrap();
    let page = store.reader().page(1, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
    assert_eq!(page.entries[0].close, fixed("9"));
}

#[tokio::test]
async fn a_prune_refuses_a_pair_the_probe_cannot_see() {
    let (path, mut store) = fresh_store_at("marks-chain-prune-unvalidated", hour_retentions());
    let market = 1;
    let base = 1_792_489_860_000;
    store
        .observe(&[
            (market, base, fixed("1")),
            (market, base + 60_000, fixed("2")),
            (market, base + 120_000, fixed("3")),
        ])
        .unwrap();
    // Capture the authentic rows the next flush will prune.
    let captured: Vec<(i64, Option<i64>, Vec<u8>)> = Connection::open(&path)
        .unwrap()
        .prepare("SELECT bucket, prev_bucket, tag FROM marks WHERE market = 1 AND resolution = 1 AND bucket < ?1")
        .unwrap()
        .query_map([base + 120_000], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(captured.len(), 2);
    // Another market's flush prunes them; the pair re-floors above where they were.
    store.observe(&[(2, base + 3_720_000, fixed("6"))]).unwrap();
    // The host restores the pruned rows, authentic tags and all, while the committed floor
    // stays above the next flush's horizon: the anchor probe cannot see the pair, but the
    // prune's plan names it, and the write refuses rather than prune a chain it never
    // validated.
    let host = Connection::open(&path).unwrap();
    for (bucket, prev, tag) in captured {
        host.execute(
            "INSERT INTO marks (market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag)
             VALUES (1, 1, ?1, '1', '1', '1', '1', 'log_derived', ?2, ?3)",
            rusqlite::params![bucket, prev, tag],
        )
        .unwrap();
    }
    drop(host);
    let err = store.observe(&[(2, base + 3_630_000, fixed("7"))]).expect_err("pruned an unvalidated pair");
    assert!(format!("{err:#}").contains("did not validate"), "{err:#}");
}

#[tokio::test]
async fn a_prune_of_an_unsampled_pair_refuses_a_tampered_head() {
    let (path, mut store) = fresh_store_at("marks-chain-unsampled-tampered", hour_retentions());
    let base = 1_792_489_860_000;
    // Market 1's pair sits above any early horizon; market 2's later flush would prune it.
    store.observe(&[(1, base + 3_600_000, fixed("1")), (1, base + 3_660_000, fixed("2"))]).unwrap();
    store.observe(&[(2, base + 60_000, fixed("5"))]).unwrap();
    // The host deletes market 1's head; its floor still sits below market 2's next horizon, so
    // the anchor probe alone puts market 1 in the validated set, and the write must refuse.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base + 3_660_000])
        .unwrap();
    let err = store
        .observe(&[(2, base + 3_660_000 + 3_600_000 + 60_000, fixed("6"))])
        .expect_err("validated nothing before pruning");
    assert!(format!("{err:#}").contains("no longer matches its committed anchor"), "{err:#}");
}

#[tokio::test]
async fn a_seed_refuses_a_prune_that_names_a_pair_it_did_not_validate() {
    let (path, mut store) = fresh_store_at("marks-chain-seed-prune-unvalidated", hour_retentions());
    let market = 1;
    let base = 1_792_489_860_000;
    store
        .observe(&[
            (market, base, fixed("1")),
            (market, base + 60_000, fixed("2")),
            (market, base + 120_000, fixed("3")),
        ])
        .unwrap();
    let captured: Vec<(i64, Option<i64>, Vec<u8>)> = Connection::open(&path)
        .unwrap()
        .prepare("SELECT bucket, prev_bucket, tag FROM marks WHERE market = 1 AND resolution = 1 AND bucket < ?1")
        .unwrap()
        .query_map([base + 120_000], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    store.observe(&[(2, base + 3_720_000, fixed("6"))]).unwrap();
    // The host restores the pruned rows below the committed floor, as in the observe twin;
    // a backfill's seed must refuse the prune the same way.
    let host = Connection::open(&path).unwrap();
    for (bucket, prev, tag) in captured {
        host.execute(
            "INSERT INTO marks (market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag)
             VALUES (1, 1, ?1, '1', '1', '1', '1', 'log_derived', ?2, ?3)",
            rusqlite::params![bucket, prev, tag],
        )
        .unwrap();
    }
    drop(host);
    let err = store.seed(&[], base + 3_630_000).expect_err("pruned an unvalidated pair");
    assert!(format!("{err:#}").contains("did not validate"), "{err:#}");
}

#[tokio::test]
async fn a_seed_pruning_an_unsampled_pair_validates_it_first() {
    let (_path, mut store) = fresh_store_at("marks-chain-seed-unsampled", hour_retentions());
    let base = 1_792_489_860_000;
    store.observe(&[(1, base, fixed("1")), (1, base + 3_600_000, fixed("2"))]).unwrap();
    store.observe(&[(2, base + 3_600_000 + 60_000, fixed("5"))]).unwrap();
    // A backfill for market 2 whose newest sweeps past market 1's floor prunes market 1.
    store
        .seed(
            &[Seed {
                market: 2,
                resolution: Resolution::M1,
                ts: base + 2 * 3_600_000,
                open: fixed("6"),
                high: fixed("6"),
                low: fixed("6"),
                close: fixed("6"),
            }],
            base + 2 * 3_600_000,
        )
        .unwrap();
    let page = store.reader().page(1, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
    assert_eq!(page.entries.len(), 1);
    store.observe(&[(1, base + 3_600_000, fixed("9"))]).unwrap();
}

#[tokio::test]
async fn a_seed_refuses_an_unsampled_pair_with_a_tampered_head() {
    let (path, mut store) = fresh_store_at("marks-chain-seed-unsampled-tampered", hour_retentions());
    let base = 1_792_489_860_000;
    store.observe(&[(1, base + 3_600_000, fixed("1")), (1, base + 3_660_000, fixed("2"))]).unwrap();
    store.observe(&[(2, base + 60_000, fixed("5"))]).unwrap();
    // The host deletes market 1's head; the seed's probe alone must put market 1 in the
    // validated set, and the seed must refuse.
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base + 3_660_000])
        .unwrap();
    let err = store
        .seed(
            &[Seed {
                market: 2,
                resolution: Resolution::M1,
                ts: base + 3_660_000 + 3_600_000 + 60_000,
                open: fixed("6"),
                high: fixed("6"),
                low: fixed("6"),
                close: fixed("6"),
            }],
            base + 3_660_000 + 3_600_000 + 60_000,
        )
        .expect_err("validated nothing before pruning");
    assert!(format!("{err:#}").contains("no longer matches its committed anchor"), "{err:#}");
}
