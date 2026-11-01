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
    assert!(format!("{err:#}").contains("recreated bucket"), "{err:#}");
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
