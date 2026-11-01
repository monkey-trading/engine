//! The prune plans from the committed anchors and reads and deletes by primary-key prefix, and
//! a read that races a commit re-synchronizes against it.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use rusqlite::named_params;

use super::*;
use crate::history::marks::store::chain::{DELETE_EXPIRED, EXPIRING, pair_anchor};
use crate::history::marks::store::reader::read_page_with;
use crate::history::marks::store::{ChainAnchors, PageQuery};

const HOUR: i64 = 3_600_000;
const BASE: i64 = 1_792_000_020_000;

/// How SQLite plans `sql` with the prune's named parameters bound.
fn plan_of(store: &Store, sql: &str) -> Vec<String> {
    let mut stmt = store.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    stmt.query_map(named_params! {":market": 1, ":resolution": 1, ":horizon": 0}, |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Every stored pair with its row count.
fn pairs_on_disk(store: &Store) -> BTreeSet<(u32, i64, i64)> {
    let mut stmt =
        store.conn.prepare("SELECT market, resolution, COUNT(*) FROM marks GROUP BY market, resolution").unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap().collect::<Result<_, _>>().unwrap()
}

/// Requires no bounded resolution to hold a row below its horizon and every committed anchor to
/// equal what the file holds.
fn assert_pruned_and_anchored(store: &Store, newest: i64) {
    let retentions = store.retentions();
    for resolution in Resolution::ALL {
        let Some(retention) = retentions.of_resolution(resolution) else { continue };
        let stale: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM marks WHERE resolution = ?1 AND bucket < ?2",
                [resolution as i64, newest - retention],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0, "{resolution:?} keeps rows below its horizon");
    }
    let anchors = store.prior_anchors();
    let on_disk: BTreeSet<(u32, Resolution)> = pairs_on_disk(store)
        .into_iter()
        .map(|(market, resolution, _)| (market, Resolution::from_tag(u8::try_from(resolution).unwrap()).unwrap()))
        .collect();
    assert_eq!(anchors.keys().copied().collect::<BTreeSet<_>>(), on_disk, "anchors name the pairs on disk");
    for (&(market, resolution), anchor) in &anchors {
        assert_eq!(pair_anchor(&store.conn, market, resolution).unwrap(), Some(*anchor), "{market} {resolution:?}");
    }
}

#[test]
fn the_prune_queries_search_the_primary_key_by_pair_prefix() {
    let store = fresh_store("marks-prune-plan", hour_retentions());
    for sql in [EXPIRING, DELETE_EXPIRED] {
        let plan = plan_of(&store, sql);
        assert!(
            plan.iter()
                .any(|step| step.contains("SEARCH marks USING PRIMARY KEY (market=? AND resolution=? AND bucket<?)")),
            "{sql}: {plan:?}"
        );
        assert!(plan.iter().all(|step| !step.contains("SCAN")), "{sql}: {plan:?}");
    }
}

#[test]
fn a_flush_prunes_every_pair_whose_floor_expired_and_nothing_else() {
    let mut store = fresh_store("marks-prune-anchors", hour_retentions());
    store
        .observe(&[(1, BASE, fixed("1")), (1, BASE + HOUR / 2, fixed("2")), (2, BASE + HOUR / 4, fixed("3"))])
        .unwrap();
    assert_pruned_and_anchored(&store, BASE + HOUR / 2);
    let kept = pairs_on_disk(&store);
    // A flush for another market moves the horizon past both idle markets' older rows: their
    // pairs are neither in the batch nor touched by it, only their committed floors expired.
    let newest = BASE + 5 * HOUR / 2;
    store.observe(&[(3, newest, fixed("4"))]).unwrap();
    assert_pruned_and_anchored(&store, newest);
    assert_ne!(pairs_on_disk(&store), kept);
    assert!(remaining(&store, 1).is_empty() && remaining(&store, 2).is_empty(), "both idle 1m pairs expired whole");
    assert_eq!(remaining(&store, 3).len(), 1);
    let d1: i64 =
        store.conn.query_row("SELECT COUNT(*) FROM marks WHERE resolution = 3", [], |row| row.get(0)).unwrap();
    assert_eq!(d1, 3, "1d is unbounded and keeps every market");
    // A pair the batch creates below the horizon, in a flush another market's sample sets, is
    // pruned in the transaction that creates it.
    let newest = BASE + 4 * HOUR;
    store.observe(&[(3, newest, fixed("5")), (4, BASE, fixed("6"))]).unwrap();
    assert_pruned_and_anchored(&store, newest);
    assert!(remaining(&store, 4).is_empty());
    // A pair the batch moves gets its older half pruned and its newer half kept.
    let newest = BASE + 6 * HOUR;
    store
        .observe(&[(3, newest - HOUR / 2, fixed("7")), (3, newest, fixed("8")), (3, newest - 3 * HOUR, fixed("9"))])
        .unwrap();
    assert_pruned_and_anchored(&store, newest);
    assert_eq!(remaining(&store, 3), [newest - HOUR / 2 - (newest - HOUR / 2) % 60_000, newest - newest % 60_000]);
}

/// The 1m buckets `market` holds, oldest first.
fn remaining(store: &Store, market: u32) -> Vec<i64> {
    store
        .conn
        .prepare("SELECT bucket FROM marks WHERE market = ?1 AND resolution = 1 ORDER BY bucket")
        .unwrap()
        .query_map([market], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Reads market `market`'s whole 1m series from a connection of `store`, running `race` on the
/// store once the first attempt holds its snapshot, `races` times at most, one per attempt. Returns
/// the attempts the read took and its outcome.
fn read_racing(
    store: &mut Store,
    market: u32,
    races: usize,
    mut race: impl FnMut(&mut Store, usize),
) -> (Vec<usize>, anyhow::Result<MarksPage>) {
    let (path, mac) = (store.path().to_owned(), store.mac.clone());
    let anchors: Arc<Mutex<ChainAnchors>> = store.anchors.clone();
    let query =
        PageQuery { market, resolution: Resolution::M1, from: i64::MIN, to: i64::MAX, limit: 100, max_limit: 100 };
    let mut attempts = Vec::new();
    let page = read_page_with(&path, &expected_schema(), &mac, &anchors, &query, &mut |attempt| {
        attempts.push(attempt);
        if attempt < races {
            race(store, attempt);
        }
    });
    (attempts, page)
}

#[test]
fn a_commit_moving_a_head_between_snapshot_and_anchor_resolves_on_the_next_attempt() {
    let mut store = fresh_store("marks-race-move", Retentions::defaults());
    store.observe(&[(1, BASE, fixed("1"))]).unwrap();
    let (attempts, page) = read_racing(&mut store, 1, 1, |store, _| {
        store.observe(&[(1, BASE + 60_000, fixed("2"))]).unwrap();
    });
    assert_eq!(attempts, [0, 1], "the raced attempt retried once");
    let page = page.unwrap();
    assert_eq!(page.entries.iter().map(|entry| entry.ts).collect::<Vec<_>>(), [BASE, BASE + 60_000]);
}

#[test]
fn a_commit_creating_the_pair_between_snapshot_and_anchor_resolves_on_the_next_attempt() {
    let mut store = fresh_store("marks-race-create", Retentions::defaults());
    store.observe(&[(2, BASE, fixed("1"))]).unwrap();
    let (attempts, page) = read_racing(&mut store, 1, 1, |store, _| {
        store.observe(&[(1, BASE, fixed("5"))]).unwrap();
    });
    assert_eq!(attempts, [0, 1]);
    assert_eq!(page.unwrap().entries.len(), 1, "the retry serves the pair the commit created");
}

#[test]
fn a_commit_removing_the_pair_between_snapshot_and_anchor_resolves_on_the_next_attempt() {
    let mut store = fresh_store("marks-race-remove", hour_retentions());
    store.observe(&[(1, BASE, fixed("1"))]).unwrap();
    // Another market's flush, two hours on, expires every 1m bucket market 1 holds.
    let (attempts, page) = read_racing(&mut store, 1, 1, |store, _| {
        store.observe(&[(2, BASE + 2 * HOUR, fixed("5"))]).unwrap();
    });
    assert_eq!(attempts, [0, 1]);
    assert!(page.unwrap().entries.is_empty(), "the retry finds the pair gone, as committed");
}

#[test]
fn two_raced_attempts_resolve_on_the_third_and_a_third_race_is_refused_as_a_rollback() {
    let mut store = fresh_store("marks-race-bounded", Retentions::defaults());
    store.observe(&[(1, BASE, fixed("1"))]).unwrap();
    let (attempts, page) = read_racing(&mut store, 1, 2, |store, attempt| {
        store.observe(&[(1, BASE + 60_000 * (attempt as i64 + 1), fixed("2"))]).unwrap();
    });
    assert_eq!(attempts, [0, 1, 2]);
    assert_eq!(page.unwrap().entries.len(), 3);
    // A commit landing in the last, strict attempt is indistinguishable from a rollback.
    let (attempts, page) = read_racing(&mut store, 1, 3, |store, attempt| {
        store.observe(&[(1, BASE + 600_000 + 60_000 * attempt as i64, fixed("3"))]).unwrap();
    });
    assert_eq!(attempts, [0, 1, 2]);
    assert!(format!("{:#}", page.unwrap_err()).contains("rolled back"));
}
