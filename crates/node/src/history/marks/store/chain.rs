//! The chained index over the marks cache: every bucket of one (market, resolution) pair names
//! the bucket below it (`prev_bucket`, NULL for the chain's floor), and every row's tag binds
//! that name, so a host that deletes, reorders, or rolls rows back breaks the chain the next
//! read walks. The writer maintains the invariant inside each transaction — validating the
//! touched pairs' committed floor and head before touching them, and re-linking only over what
//! the transaction itself deleted or inserted — because a chain that no longer matches its
//! committed anchor, or a predecessor reference the writer did not move, is host tampering that
//! reads must keep seeing, never something the writer heals.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, bail, ensure};
use engine_core::types::{MarketId, Millis};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::history::marks::Resolution;
use crate::history::marks::store::auth::{Row, tag};
use crate::history::marks::store::{ChainAnchor, ChainAnchors, ChainHead, Seed, SnapRow};
use crate::history::marks::{Provenance, Retentions};

/// The tag a stored row must carry: an HMAC of every stored field, including the chain link, so
/// a row the host rewrote fails verification before it can be served, merged, or re-linked.
/// Error cases:
/// - the tag does not match the stored fields
pub(crate) fn verify_row(
    mac: &hmac::Hmac<sha2::Sha256>,
    market: MarketId,
    resolution: Resolution,
    row: &SnapRow,
) -> anyhow::Result<()> {
    let expected = tag(
        mac,
        market,
        resolution,
        row.bucket,
        &Row {
            open: &row.open,
            high: &row.high,
            low: &row.low,
            close: &row.close,
            provenance: &row.provenance,
            prev: row.prev,
        },
    );
    ensure!(row.tag == expected, "the {market} {resolution:?} bucket at {} fails authentication", row.bucket);
    Ok(())
}

/// Re-links `row` to `prev`, keeping its content: updates the chain link and the tag that binds
/// it, in the transaction that reasons over the chain.
/// Error cases:
/// - the update fails
pub(crate) fn retag(
    mac: &hmac::Hmac<sha2::Sha256>,
    tx: &rusqlite::Transaction,
    market: MarketId,
    resolution: Resolution,
    row: &SnapRow,
    prev: Option<Millis>,
) -> anyhow::Result<()> {
    let tag = tag(
        mac,
        market,
        resolution,
        row.bucket,
        &Row { open: &row.open, high: &row.high, low: &row.low, close: &row.close, provenance: &row.provenance, prev },
    );
    tx.execute(
        "UPDATE marks SET prev_bucket = ?4, tag = ?5 WHERE market = ?1 AND resolution = ?2 AND bucket = ?3",
        params![i64::from(market), resolution as i64, row.bucket, prev, tag],
    )
    .with_context(|| format!("re-linking the {market} {resolution:?} bucket at {}", row.bucket))?;
    Ok(())
}

/// One stored row of one pair, for the merge's read of the bucket a sample lands in.
pub(crate) const ROW_AT: &str = "SELECT open, high, low, close, provenance, prev_bucket, tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket = :bucket";
/// The greatest bucket below `bucket`, the chain link a stored or new row there must name.
pub(crate) const PREDECESSOR: &str = "SELECT MAX(bucket) FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket < :bucket";
/// The least bucket above `bucket`, whose link a fresh insert can leave stale: only the floor's
/// NULL or a gap the insert fills may change, and only to the insert itself.
pub(crate) const SUCCESSOR: &str = "SELECT bucket, open, high, low, close, provenance, prev_bucket, tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket > :bucket ORDER BY bucket ASC LIMIT 1";
/// Whether a bucket holds a row, for a successor link that names a bucket below the insert.
pub(crate) const EXISTS: &str =
    "SELECT EXISTS(SELECT 1 FROM marks WHERE market = :market AND resolution = :resolution AND bucket = :bucket)";
/// One pair's floor row, half of its committed anchor.
const FLOOR: &str = "SELECT bucket, tag FROM marks
    WHERE market = :market AND resolution = :resolution ORDER BY bucket ASC LIMIT 1";
/// One pair's head row, half of its committed anchor.
const HEAD: &str = "SELECT bucket, tag FROM marks
    WHERE market = :market AND resolution = :resolution ORDER BY bucket DESC LIMIT 1";

/// The anchor updates one write publishes with its commit: `Some` for a pair's freshly committed
/// floor and head, `None` for a pair whose every bucket the write removed.
pub(crate) type ChainUpdates = BTreeMap<(MarketId, Resolution), Option<ChainAnchor>>;

/// One resolution's planned deletion: its horizon and the rows below it, in market, bucket
/// order.
pub(crate) type PrunePass = (Resolution, Millis, Vec<(MarketId, SnapRow)>, Vec<MarketId>);

/// The buckets one prune pass will delete, per resolution: planned before any mutation so the
/// pairs a write touches, and validates, are known up front.
pub(crate) struct PrunePlan {
    passes: Vec<PrunePass>,
}

/// The committed floor and head of one pair as the file holds them now; `None` when the pair has
/// no rows.
/// Error cases:
/// - the floor or head cannot be read
pub(crate) fn pair_anchor(
    conn: &Connection,
    market: MarketId,
    resolution: Resolution,
) -> anyhow::Result<Option<ChainAnchor>> {
    let floor: Option<(Millis, [u8; 16])> = conn
        .query_row(FLOOR, params![i64::from(market), resolution as i64], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()
        .with_context(|| format!("reading the {market} {resolution:?} chain floor"))?;
    let Some((floor_bucket, floor_tag)) = floor else { return Ok(None) };
    let (head_bucket, head_tag): (Millis, [u8; 16]) = conn
        .query_row(HEAD, params![i64::from(market), resolution as i64], |row| Ok((row.get(0)?, row.get(1)?)))
        .with_context(|| format!("reading the {market} {resolution:?} chain head"))?;
    Ok(Some(ChainAnchor {
        floor: ChainHead { bucket: floor_bucket, tag: floor_tag },
        head: ChainHead { bucket: head_bucket, tag: head_tag },
    }))
}

/// Requires every touched pair's floor and head to equal its committed anchor: the write is
/// about to build on these chains, so a file that no longer matches what this node committed is
/// refused before anything changes, never adopted.
/// Error cases:
/// - a pair's floor or head does not match its committed anchor
pub(crate) fn check_anchors(
    tx: &Transaction,
    prior: &ChainAnchors,
    pairs: &BTreeSet<(MarketId, Resolution)>,
) -> anyhow::Result<()> {
    for (market, resolution) in pairs {
        let current = pair_anchor(tx, *market, *resolution)?;
        ensure!(
            current == prior.get(&(*market, *resolution)).copied(),
            "the {market} {resolution:?} chain no longer matches its committed anchor: its floor or head changed outside a write"
        );
    }
    Ok(())
}

/// Derives the anchor updates for the touched pairs from the transaction's own post-write view,
/// so the committed anchors describe exactly what the transaction wrote, never unvalidated disk.
/// Error cases:
/// - a pair's floor or head cannot be read
pub(crate) fn anchor_updates(
    tx: &Transaction,
    pairs: &BTreeSet<(MarketId, Resolution)>,
) -> anyhow::Result<ChainUpdates> {
    pairs
        .iter()
        .map(|(market, resolution)| Ok(((*market, *resolution), pair_anchor(tx, *market, *resolution)?)))
        .collect()
}

/// The pairs whose committed floor sits below a resolution's prune horizon, counted back from
/// `newest`: what a write must validate before it mutates. In-memory over the committed anchors,
/// so the write pays nothing to pairs with nothing to prune; the full plan the prune applies is
/// built after the write's own mutations (see `plan_prune`).
pub(crate) fn prune_markets(
    prior: &ChainAnchors,
    retentions: &Retentions,
    newest: Millis,
) -> BTreeSet<(MarketId, Resolution)> {
    prior
        .iter()
        .filter(|((_, resolution), anchor)| {
            retentions
                .of_resolution(*resolution)
                .is_some_and(|retention| anchor.floor.bucket < newest.saturating_sub(retention))
        })
        .map(|(pair, _)| *pair)
        .collect()
}

/// Requires every pair a prune plan touches to be among the pairs the write validated before
/// mutating anything. On an untampered file the probe and the samples already cover them all; a
/// pair appearing only in the plan means the file changed under the probe, which the write
/// refuses, never prunes unchecked.
/// Error cases:
/// - the plan names a pair outside the validated set
pub(crate) fn ensure_planned_pairs_validated(
    plan: &PrunePlan,
    touched: &BTreeSet<(MarketId, Resolution)>,
) -> anyhow::Result<()> {
    for (resolution, _, _, markets) in &plan.passes {
        for market in markets {
            ensure!(
                touched.contains(&(*market, *resolution)),
                "the prune touches the {market} {resolution:?} pair, which this write did not validate"
            );
        }
    }
    Ok(())
}

/// Plans the buckets each bounded resolution will prune, counted back from `newest`: which rows
/// die is decided before anything mutates, so the pairs the prune touches can be validated with
/// the rest of the write. Planning reads only; `apply_prune` does the deleting.
/// Error cases:
/// - the plan cannot be read
pub(crate) fn plan_prune(tx: &Transaction, retentions: &Retentions, newest: Millis) -> anyhow::Result<PrunePlan> {
    let mut passes = Vec::new();
    for resolution in Resolution::ALL {
        let Some(retention) = retentions.of_resolution(resolution) else { continue };
        let horizon = newest.saturating_sub(retention);
        let mut rows = tx
            .prepare(
                "SELECT market, bucket, open, high, low, close, provenance, prev_bucket, tag FROM marks
                 WHERE resolution = ?1 AND bucket < ?2 ORDER BY market, bucket",
            )
            .with_context(|| format!("planning the {resolution:?} prune"))?;
        let rows = rows
            .query_map(params![resolution as i64, horizon], |row| {
                Ok((
                    row.get::<_, MarketId>(0)?,
                    SnapRow {
                        bucket: row.get(1)?,
                        open: row.get(2)?,
                        high: row.get(3)?,
                        low: row.get(4)?,
                        close: row.get(5)?,
                        provenance: row.get(6)?,
                        prev: row.get(7)?,
                        tag: row.get(8)?,
                    },
                ))
            })
            .with_context(|| format!("planning the {resolution:?} prune"))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("planning the {resolution:?} prune"))?;
        let markets: Vec<MarketId> =
            rows.iter().map(|(market, _)| *market).collect::<BTreeSet<_>>().into_iter().collect();
        passes.push((resolution, horizon, rows, markets));
    }
    Ok(PrunePlan { passes })
}

/// Deletes the planned buckets and re-floors each pair whose floor pruned away. The expiring
/// rows are a pair's prefix, and the delete removes them and every reference to them: each must
/// authenticate and name the row actually below it, or the delete would erase the last record of
/// a host deletion — including a record the host edited into a row that no longer verifies. A
/// surviving floor's link may be cleared only when it names a bucket this delete removed; a link
/// naming any other missing bucket is a host deletion this write refuses, never heals.
/// Error cases:
/// - the delete fails, an expiring or floor row fails authentication, a link names a bucket that
///   is already missing, or a floor link names a bucket this prune did not delete
pub(crate) fn apply_prune(mac: &hmac::Hmac<sha2::Sha256>, tx: &Transaction, plan: &PrunePlan) -> anyhow::Result<()> {
    for (resolution, horizon, rows, markets) in &plan.passes {
        if rows.is_empty() {
            continue;
        }
        let mut market = None;
        let mut previous = None;
        for (row_market, row) in rows {
            if market != Some(*row_market) {
                market = Some(*row_market);
                previous = None;
            }
            // Authenticate before trusting the link: an edited row fails here, not by healing.
            verify_row(mac, *row_market, *resolution, row)?;
            ensure!(
                row.prev == previous,
                "the {row_market} {resolution:?} chain is broken at {}: its link names a bucket that is already missing",
                row.bucket
            );
            previous = Some(row.bucket);
        }
        let mut last_deleted: BTreeMap<MarketId, Millis> = BTreeMap::new();
        for (market, row) in rows {
            last_deleted.entry(*market).and_modify(|last| *last = (*last).max(row.bucket)).or_insert(row.bucket);
        }
        tx.execute("DELETE FROM marks WHERE resolution = ?1 AND bucket < ?2", params![*resolution as i64, horizon])
            .with_context(|| format!("pruning {resolution:?} marks"))?;
        // A pair that had rows below the horizon must survive at a floor whose link names the
        // last deleted bucket: every other survivor's predecessor is at or above the horizon and
        // still there. A NULL link or a link skipping that bucket is a restored snapshot row,
        // refused, not published as the new floor. Each affected pair's floor is one lookup.
        let mut floor = tx.prepare(
            "SELECT bucket, open, high, low, close, provenance, prev_bucket, tag FROM marks
             WHERE market = :market AND resolution = :resolution ORDER BY bucket ASC LIMIT 1",
        )?;
        for market in markets {
            let floor: Option<SnapRow> = floor
                .query_row(params![i64::from(*market), *resolution as i64], |row| {
                    Ok(SnapRow {
                        bucket: row.get(0)?,
                        open: row.get(1)?,
                        high: row.get(2)?,
                        low: row.get(3)?,
                        close: row.get(4)?,
                        provenance: row.get(5)?,
                        prev: row.get(6)?,
                        tag: row.get(7)?,
                    })
                })
                .optional()
                .with_context(|| format!("reading the pruned {resolution:?} floor of {market}"))?;
            let Some(floor) = floor else { continue };
            let last = last_deleted.get(market).copied().expect("the market had rows deleted");
            verify_row(mac, *market, *resolution, &floor)?;
            let Some(prev) = floor.prev else {
                bail!(
                    "the {market} {resolution:?} chain is broken at {}: its floor names nothing, below buckets this prune deleted",
                    floor.bucket
                );
            };
            ensure!(
                prev == last,
                "the {market} {resolution:?} chain is broken at {}: its floor names the bucket {prev}, not the last this prune deleted",
                floor.bucket
            );
            retag(mac, tx, *market, *resolution, &floor, None)?;
        }
    }
    Ok(())
}

/// Seeds `rows`, provider candles turned into buckets, keeping every row already there: seeded
/// history never overwrites log-derived marks or an earlier seed. Inserts run oldest first, each
/// naming the bucket actually below it, and only the pre-existing row above an insert can need
/// re-linking — to the insert itself, never over a bucket that vanished some other way. Returns
/// how many rows were added.
/// Error cases:
/// - the transaction fails, a stored row fails authentication, or a link names a bucket this
///   seed did not insert
pub(crate) fn seed(mac: &hmac::Hmac<sha2::Sha256>, tx: &Transaction, rows: &[Seed]) -> anyhow::Result<usize> {
    let mut by_pair: BTreeMap<(MarketId, Resolution), Vec<&Seed>> = BTreeMap::new();
    for row in rows {
        by_pair.entry((row.market, row.resolution)).or_default().push(row);
    }
    let mut inserted = 0;
    for ((market, resolution), mut seeds) in by_pair {
        seeds.sort_by_key(|seed| seed.ts);
        let mut fresh: BTreeSet<Millis> = BTreeSet::new();
        for seed in seeds {
            if fresh.contains(&seed.ts) {
                continue;
            }
            let exists: i64 = tx
                .query_row(EXISTS, params![i64::from(market), resolution as i64, seed.ts], |row| row.get(0))
                .with_context(|| format!("reading the {market} {resolution:?} chain at {}", seed.ts))?;
            if exists == 1 {
                continue;
            }
            let pred: Option<Millis> = tx
                .query_row(PREDECESSOR, params![i64::from(market), resolution as i64, seed.ts], |row| row.get(0))
                .with_context(|| format!("reading the {market} {resolution:?} chain at {}", seed.ts))?;
            let provenance = Provenance::SeededFromProvider.as_str();
            let (open, high, low, close) =
                (seed.open.to_string(), seed.high.to_string(), seed.low.to_string(), seed.close.to_string());
            let tag = tag(
                mac,
                market,
                resolution,
                seed.ts,
                &Row { open: &open, high: &high, low: &low, close: &close, provenance, prev: pred },
            );
            inserted += tx
                .execute(
                    "INSERT INTO marks (market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        i64::from(market),
                        resolution as i64,
                        seed.ts,
                        open,
                        high,
                        low,
                        close,
                        provenance,
                        pred,
                        tag
                    ],
                )
                .with_context(|| format!("seeding {market} {resolution:?} at {}", seed.ts))?;
            fresh.insert(seed.ts);
        }
        // Only the pre-existing row directly above an insert can have a stale link: the rows
        // this seed inserted chained to each other as they went in.
        let mut stmts = crate::history::marks::store::merge::MergeStmts {
            row_at: tx.prepare(ROW_AT).context("preparing a marks relink")?,
            predecessor: tx.prepare(PREDECESSOR).context("preparing a marks relink")?,
            insert: tx
                .prepare(
                    "INSERT OR REPLACE INTO marks (market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                )
                .context("preparing a marks relink")?,
            successor: tx.prepare(SUCCESSOR).context("preparing a marks relink")?,
        };
        for bucket in fresh.iter().copied() {
            crate::history::marks::store::merge::relink_successor(
                mac, tx, &mut stmts, market, resolution, bucket, &fresh,
            )?;
        }
    }
    Ok(inserted)
}
