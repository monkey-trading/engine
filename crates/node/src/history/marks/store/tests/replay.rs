//! Replayed interior rows: an older authentic version of a row with a row above it, put back by
//! the host while the node runs, is refused at serve time — whether the row was rewritten after
//! the row above arrived (a gap filled under it, a late sample merged into it), which the
//! committed anchors pin by tag, or only grew while it was the head, which the row above vouches
//! for by the tag it recorded.

use rusqlite::Connection;
use rusqlite::types::Value;

use super::*;

const BASE: i64 = 1_792_489_860_000;
const MIN: i64 = 60_000;
const HOUR: i64 = 3_600_000;
const COLUMNS: &str = "bucket, open, high, low, close, provenance, prev_bucket, tag, prev_tag";

/// Every stored column of one 1m row of market 1, as the host would copy it.
fn capture(path: &std::path::Path, bucket: i64) -> Vec<Value> {
    Connection::open(path)
        .unwrap()
        .query_row(
            &format!("SELECT {COLUMNS} FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1"),
            [bucket],
            |row| (0..9).map(|n| row.get::<_, Value>(n)).collect(),
        )
        .unwrap()
}

/// The host puts a copied row back, over whatever now holds its bucket.
fn restore(path: &std::path::Path, row: &[Value]) {
    Connection::open(path)
        .unwrap()
        .execute(
            &format!("INSERT OR REPLACE INTO marks (market, resolution, {COLUMNS}) VALUES (1, 1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"),
            rusqlite::params_from_iter(row),
        )
        .unwrap();
}

fn delete(path: &std::path::Path, bucket: i64) {
    Connection::open(path)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [bucket])
        .unwrap();
}

fn seed_at(ts: i64, price: &str) -> Seed {
    let price = fixed(price);
    Seed { market: 1, resolution: Resolution::M1, ts, open: price, high: price, low: price, close: price }
}

async fn page(store: &Store, from: i64, to: i64) -> anyhow::Result<MarksPage> {
    store.reader().page(1, Resolution::M1, from, to, 100, 100).await
}

/// Every window refuses, naming a replayed row.
async fn assert_refused(store: &Store, windows: &[(i64, i64)]) {
    for &(from, to) in windows {
        let err = page(store, from, to).await.expect_err("served a replayed row");
        assert!(
            ["older version", "bound to another version"].iter().any(|m| format!("{err:#}").contains(m)),
            "{from}..{to}: {err:#}"
        );
    }
}

#[tokio::test]
async fn a_gap_filled_under_a_row_cannot_be_undone_by_restoring_the_row() {
    let (path, mut store) = fresh_store_at("marks-replay-gap", Retentions::defaults());
    // Live marks write A and C, and the host captures C's row; D follows, so C is interior.
    store.observe(&[(1, BASE, fixed("1")), (1, BASE + 2 * MIN, fixed("3"))]).unwrap();
    let captured = capture(&path, BASE + 2 * MIN);
    store.observe(&[(1, BASE + 3 * MIN, fixed("4"))]).unwrap();
    // A backfill inserts B and C re-links to B.
    store.seed(&[seed_at(BASE + MIN, "2")], BASE + 3 * MIN).unwrap();
    assert_eq!(page(&store, i64::MIN, i64::MAX).await.unwrap().entries.len(), 4);
    // The host deletes B and puts C's captured row back: neighbors unchanged, every tag and link
    // authentic on its own.
    delete(&path, BASE + MIN);
    restore(&path, &captured);
    // Any window holding C refuses, and so does one that ends below it and would hide B: the row
    // above a window vouches for the span it closes.
    let c = BASE + 2 * MIN;
    assert_refused(&store, &[(i64::MIN, i64::MAX), (c, c), (BASE, c), (c, BASE + 10 * MIN), (BASE, BASE + MIN)]).await;
    assert_refused(&store, &[(BASE, BASE)]).await;
}

#[tokio::test]
async fn an_earlier_ohlc_of_a_bucket_that_grew_while_it_was_the_head_is_refused() {
    let (path, mut store) = fresh_store_at("marks-replay-grown", Retentions::defaults());
    store.observe(&[(1, BASE, fixed("1")), (1, BASE + MIN, fixed("5"))]).unwrap();
    let early = capture(&path, BASE + MIN);
    store.observe(&[(1, BASE + MIN, fixed("9"))]).unwrap();
    store.observe(&[(1, BASE + 2 * MIN, fixed("4"))]).unwrap();
    assert!(page(&store, i64::MIN, i64::MAX).await.is_ok());
    // The head moved on, so the row above vouches for the bucket's final content.
    restore(&path, &early);
    let at = BASE + MIN;
    assert_refused(&store, &[(i64::MIN, i64::MAX), (at, at), (BASE, at), (at, BASE + 10 * MIN)]).await;
}

#[tokio::test]
async fn an_earlier_version_of_a_row_a_late_sample_merged_into_is_refused() {
    let (path, mut store) = fresh_store_at("marks-replay-late", Retentions::defaults());
    store.observe(&[(1, BASE, fixed("1")), (1, BASE + MIN, fixed("5")), (1, BASE + 2 * MIN, fixed("4"))]).unwrap();
    // The row above recorded B's tag as it stands now; a late sample then rewrites B.
    let before = capture(&path, BASE + MIN);
    store.observe(&[(1, BASE + MIN + 1_000, fixed("9"))]).unwrap();
    let after = page(&store, BASE + MIN, BASE + MIN).await.unwrap();
    assert_eq!(after.entries[0].high, fixed("9"));
    // Restoring the version the row above recorded passes that row's check; the pin refuses it.
    restore(&path, &before);
    let at = BASE + MIN;
    assert_refused(&store, &[(i64::MIN, i64::MAX), (at, at), (BASE, at), (at, BASE + 10 * MIN), (BASE, at - 1)]).await;
}

#[tokio::test]
async fn head_merges_pin_nothing_and_a_prune_drops_the_pins_below_the_floor() {
    let (_path, mut store) = fresh_store_at("marks-replay-pins", hour_retentions());
    let pinned = |store: &Store| store.prior_anchors().interior((1, Resolution::M1)).len();
    let samples: Vec<_> = (0..40).map(|n| (1, BASE + n * 15_000, fixed(&format!("{}", 1 + n % 7)))).collect();
    for sample in &samples {
        store.observe(&[*sample]).unwrap();
    }
    assert_eq!(pinned(&store), 0, "live marks at the head pin nothing");
    // A backfill under the floor pins the row it re-links, and the next prune that passes it drops the pin.
    store.seed(&[seed_at(BASE - MIN, "1"), seed_at(BASE - 2 * MIN, "1")], BASE).unwrap();
    assert_eq!(pinned(&store), 1);
    store.observe(&[(1, BASE + 2 * HOUR, fixed("3"))]).unwrap();
    let pins = store.prior_anchors().interior((1, Resolution::M1));
    assert!(pins.keys().all(|bucket| *bucket >= BASE + 2 * HOUR), "pins below the floor are dropped: {pins:?}");
}

#[tokio::test]
async fn a_reset_re_binds_the_survivors_and_clears_the_pins() {
    let (_path, mut store) = fresh_store_at("marks-replay-reset", Retentions::defaults());
    store.observe(&[(1, BASE, fixed("1")), (1, BASE + 3 * MIN, fixed("4"))]).unwrap();
    store.observe(&[(1, BASE + 4 * MIN, fixed("5"))]).unwrap();
    store.seed(&[seed_at(BASE + 2 * MIN, "3"), seed_at(BASE + MIN, "2")], BASE + 4 * MIN).unwrap();
    assert!(!store.prior_anchors().interior((1, Resolution::M1)).is_empty());
    store.reset_log_derived().unwrap();
    assert!(store.prior_anchors().interior((1, Resolution::M1)).is_empty());
    let entries = page(&store, i64::MIN, i64::MAX).await.unwrap().entries;
    assert_eq!(entries.iter().map(|entry| entry.ts).collect::<Vec<_>>(), [BASE + MIN, BASE + 2 * MIN]);
}

/// The issue's scenario to the letter: C is still the head when the backfill inserts B, so the
/// re-link of C is not covered by a successor's record; D arrives after.
#[tokio::test]
async fn a_head_re_linked_by_a_backfill_cannot_be_restored_once_a_row_follows() {
    let (path, mut store) = fresh_store_at("marks-replay-issue", Retentions::defaults());
    store.observe(&[(1, BASE, fixed("1")), (1, BASE + 2 * MIN, fixed("3"))]).unwrap();
    let captured = capture(&path, BASE + 2 * MIN);
    store.seed(&[seed_at(BASE + MIN, "2")], BASE + 2 * MIN).unwrap();
    store.observe(&[(1, BASE + 3 * MIN, fixed("4"))]).unwrap();
    assert_eq!(page(&store, i64::MIN, i64::MAX).await.unwrap().entries.len(), 4);
    delete(&path, BASE + MIN);
    restore(&path, &captured);
    let c = BASE + 2 * MIN;
    assert_refused(&store, &[(i64::MIN, i64::MAX), (c, c), (BASE, c), (BASE, BASE + MIN), (BASE, BASE)]).await;
}

/// The writer does not bless a restored row by re-tagging it: a prune that re-floors it refuses.
#[tokio::test]
async fn a_prune_refuses_to_re_floor_a_restored_pinned_row() {
    let (path, mut store) = fresh_store_at("marks-replay-prune-pinned", hour_retentions());
    store.observe(&[(1, BASE, fixed("1")), (1, BASE + 2 * MIN, fixed("3"))]).unwrap();
    let captured = capture(&path, BASE + 2 * MIN);
    store.observe(&[(1, BASE + 3 * MIN, fixed("4"))]).unwrap();
    store.seed(&[seed_at(BASE + MIN, "2")], BASE + 3 * MIN).unwrap();
    delete(&path, BASE + MIN);
    restore(&path, &captured);
    // The flush expires A only; C's link names it, so only the pin stands between C and the floor.
    let err = store.observe(&[(1, BASE + HOUR + 30_000, fixed("5"))]).expect_err("re-floored a restored row");
    assert!(format!("{err:#}").contains("not the version committed for it"), "{err:#}");
}

/// The same with no pin: the restored row is an earlier version of a bucket that grew as the head,
/// and only the row above it knows.
#[tokio::test]
async fn a_prune_refuses_to_re_floor_an_earlier_version_of_a_grown_bucket() {
    let (path, mut store) = fresh_store_at("marks-replay-prune-grown", hour_retentions());
    store.observe(&[(1, BASE, fixed("1")), (1, BASE + MIN, fixed("5"))]).unwrap();
    let early = capture(&path, BASE + MIN);
    store.observe(&[(1, BASE + MIN, fixed("9"))]).unwrap();
    store.observe(&[(1, BASE + 2 * MIN, fixed("4"))]).unwrap();
    restore(&path, &early);
    let err = store.observe(&[(1, BASE + HOUR + 30_000, fixed("5"))]).expect_err("re-floored a stale row");
    assert!(format!("{err:#}").contains("not the version the row above it recorded"), "{err:#}");
}

#[tokio::test]
async fn a_late_sample_refuses_to_merge_into_an_earlier_version_of_the_bucket() {
    let (path, mut store) = fresh_store_at("marks-replay-merge-stale", Retentions::defaults());
    store.observe(&[(1, BASE, fixed("1")), (1, BASE + MIN, fixed("5"))]).unwrap();
    let early = capture(&path, BASE + MIN);
    store.observe(&[(1, BASE + MIN, fixed("9"))]).unwrap();
    store.observe(&[(1, BASE + 2 * MIN, fixed("4"))]).unwrap();
    restore(&path, &early);
    let err = store.observe(&[(1, BASE + MIN + 5_000, fixed("6"))]).expect_err("merged into a stale row");
    assert!(format!("{err:#}").contains("not the version the row above it recorded"), "{err:#}");
}

#[tokio::test]
async fn a_backfill_refuses_to_bind_the_tag_of_an_earlier_version() {
    let (path, mut store) = fresh_store_at("marks-replay-seed-stale", Retentions::defaults());
    store.observe(&[(1, BASE, fixed("1")), (1, BASE + MIN, fixed("5"))]).unwrap();
    let early = capture(&path, BASE + MIN);
    store.observe(&[(1, BASE + MIN, fixed("9"))]).unwrap();
    store.observe(&[(1, BASE + 5 * MIN, fixed("4"))]).unwrap();
    restore(&path, &early);
    // G would record the restored row's tag as its predecessor's, hiding the substitution.
    let err = store.seed(&[seed_at(BASE + 3 * MIN, "2")], BASE + 5 * MIN).expect_err("bound a stale predecessor");
    assert!(format!("{err:#}").contains("not the version the row above it recorded"), "{err:#}");
}

#[tokio::test]
async fn a_prune_pins_the_new_floor_and_pages_across_it_serve() {
    let (_path, mut store) = fresh_store_at("marks-replay-prune-floor", hour_retentions());
    store.observe(&[(1, BASE, fixed("1")), (1, BASE + MIN, fixed("2")), (1, BASE + 2 * MIN, fixed("3"))]).unwrap();
    store.observe(&[(1, BASE + HOUR + 30_000, fixed("4"))]).unwrap();
    let entries = page(&store, i64::MIN, i64::MAX).await.unwrap().entries;
    assert_eq!(entries.iter().map(|entry| entry.ts).collect::<Vec<_>>(), [BASE + MIN, BASE + 2 * MIN, BASE + HOUR]);
    assert!(store.prior_anchors().interior((1, Resolution::M1)).contains_key(&(BASE + MIN)), "the floor is pinned");
}

/// A restart imports the prior run's rows without its pins; the re-bind that startup runs makes
/// the chain readable again.
#[tokio::test]
async fn an_imported_chain_with_rewritten_rows_reads_after_the_startup_re_bind() {
    let prior_dir = scratch_dir("marks-replay-import-prior");
    let mut prior = Store::open(&prior_dir.join("marks.sqlite"), Retentions::defaults(), mac_key()).unwrap();
    prior.observe(&[(1, BASE, fixed("1")), (1, BASE + 2 * MIN, fixed("3"))]).unwrap();
    prior.observe(&[(1, BASE + 3 * MIN, fixed("4"))]).unwrap();
    prior.seed(&[seed_at(BASE + MIN, "2")], BASE + 3 * MIN).unwrap();
    assert!(!prior.prior_anchors().interior((1, Resolution::M1)).is_empty());
    drop(prior);
    let mut store = fresh_store("marks-replay-import-fresh", Retentions::defaults());
    store.import_prior(&prior_dir.join("marks.sqlite")).unwrap();
    store.validate_chains().unwrap();
    assert!(store.prior_anchors().interior((1, Resolution::M1)).is_empty());
    assert_eq!(page(&store, i64::MIN, i64::MAX).await.unwrap().entries.len(), 4);
}
