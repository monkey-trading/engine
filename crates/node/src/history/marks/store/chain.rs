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
use engine_core::Fixed;
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
const ROW_AT: &str = "SELECT open, high, low, close, provenance, prev_bucket, tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket = :bucket";
/// The greatest bucket below `bucket`, the chain link a stored or new row there must name.
const PREDECESSOR: &str = "SELECT MAX(bucket) FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket < :bucket";
/// The least bucket above `bucket`, whose link a fresh insert can leave stale: only the floor's
/// NULL or a gap the insert fills may change, and only to the insert itself.
const SUCCESSOR: &str = "SELECT bucket, open, high, low, close, provenance, prev_bucket, tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket > :bucket ORDER BY bucket ASC LIMIT 1";
/// Whether a bucket holds a row, for a successor link that names a bucket below the insert.
const EXISTS: &str =
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

/// One resolution's planned deletion: its horizon and the (market, bucket) rows below it.
pub(crate) type PrunePass = (Resolution, Millis, Vec<(MarketId, Millis)>);

/// The buckets one prune pass will delete, per resolution: planned before any mutation so the
/// pairs a write touches, and validates, are known up front.
pub(crate) struct PrunePlan {
    passes: Vec<PrunePass>,
}

impl PrunePlan {
    /// Every (market, resolution) pair the plan deletes rows from.
    pub(crate) fn touched_pairs(&self) -> BTreeSet<(MarketId, Resolution)> {
        self.passes
            .iter()
            .flat_map(|(resolution, _, rows)| rows.iter().map(move |(market, _)| (*market, *resolution)))
            .collect()
    }
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
            .prepare("SELECT market, bucket FROM marks WHERE resolution = ?1 AND bucket < ?2")
            .with_context(|| format!("planning the {resolution:?} prune"))?;
        let rows = rows
            .query_map(params![resolution as i64, horizon], |row| {
                Ok((row.get::<_, MarketId>(0)?, row.get::<_, Millis>(1)?))
            })
            .with_context(|| format!("planning the {resolution:?} prune"))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("planning the {resolution:?} prune"))?;
        passes.push((resolution, horizon, rows));
    }
    Ok(PrunePlan { passes })
}

/// Deletes the planned buckets and re-floors each pair whose floor pruned away. A surviving
/// floor's link may be cleared only when it names a bucket this delete removed; a link naming any
/// other missing bucket is a host deletion this write refuses, never heals.
/// Error cases:
/// - the delete fails, a floor row fails authentication, or a floor link names a bucket this
///   prune did not delete
pub(crate) fn apply_prune(mac: &hmac::Hmac<sha2::Sha256>, tx: &Transaction, plan: &PrunePlan) -> anyhow::Result<()> {
    for (resolution, horizon, rows) in &plan.passes {
        if rows.is_empty() {
            continue;
        }
        let deleted: BTreeSet<(MarketId, Millis)> = rows.iter().copied().collect();
        tx.execute("DELETE FROM marks WHERE resolution = ?1 AND bucket < ?2", params![*resolution as i64, horizon])
            .with_context(|| format!("pruning {resolution:?} marks"))?;
        // Only a pair's floor can name a pruned bucket: every other survivor's predecessor is at
        // or above the horizon and still there.
        let mut floors = tx.prepare(
            "SELECT m.market, m.bucket, m.open, m.high, m.low, m.close, m.provenance, m.prev_bucket, m.tag
             FROM marks m
             JOIN (SELECT market, MIN(bucket) AS floor FROM marks WHERE resolution = ?1 GROUP BY market) f
             ON m.market = f.market AND m.bucket = f.floor
             WHERE m.resolution = ?1",
        )?;
        let floors = floors
            .query_map(params![*resolution as i64], |row| {
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
            .with_context(|| format!("reading the pruned {resolution:?} floors"))?;
        for (market, floor) in floors
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("reading the pruned {resolution:?} floors"))?
        {
            let Some(prev) = floor.prev else { continue };
            ensure!(
                deleted.contains(&(market, prev)),
                "the {market} {resolution:?} chain is broken at {}: its floor names the bucket {prev}, which this prune did not delete",
                floor.bucket
            );
            verify_row(mac, market, *resolution, &floor)?;
            retag(mac, tx, market, *resolution, &floor, None)?;
        }
    }
    Ok(())
}

/// Merges `mark` into the `(market, resolution, bucket)` row: kept open, widened high and low,
/// moved close, and the bucket now log-derived. A fresh bucket takes the chain link naming the
/// bucket below it and repairs the floor or gap link above it; a row the host altered — to other
/// prices, another provenance, an unparseable value, or a broken chain link — fails the write,
/// which then changes nothing.
/// Error cases:
/// - the transaction fails, a stored price does not parse, or a chain link no longer resolves
pub(crate) fn merge(
    mac: &hmac::Hmac<sha2::Sha256>,
    tx: &Transaction,
    market: MarketId,
    resolution: Resolution,
    bucket: Millis,
    mark: Fixed,
) -> anyhow::Result<()> {
    let row = tx
        .query_row(ROW_AT, params![i64::from(market), resolution as i64, bucket], |row| {
            Ok(SnapRow {
                bucket,
                open: row.get(0)?,
                high: row.get(1)?,
                low: row.get(2)?,
                close: row.get(3)?,
                provenance: row.get(4)?,
                prev: row.get(5)?,
                tag: row.get(6)?,
            })
        })
        .optional()
        .with_context(|| format!("reading the {market} {resolution:?} bucket at {bucket}"))?;
    let pred: Option<Millis> = tx
        .query_row(PREDECESSOR, params![i64::from(market), resolution as i64, bucket], |row| row.get(0))
        .with_context(|| format!("reading the {market} {resolution:?} chain at {bucket}"))?;
    let (open, high, low, close, prev) = match &row {
        None => {
            let text = mark.to_string();
            (text.clone(), text.clone(), text.clone(), text, pred)
        }
        Some(row) => {
            // A row whose tag does not match its stored fields, or whose link names a bucket that
            // is not the one actually below it, was rewritten or undermined; refuse to build on
            // it, so host edits never reach a merge or a signed answer.
            verify_row(mac, market, resolution, row)
                .with_context(|| format!("reading the {market} {resolution:?} bucket at {bucket}"))?;
            ensure!(
                row.prev == pred,
                "the {market} {resolution:?} chain is broken at {bucket}: its link names a bucket that is no longer below it"
            );
            if row.provenance != Provenance::LogDerived.as_str() {
                // A provider-seeded bucket the log now speaks for is rebuilt from the log mark
                // alone, so a log-derived bucket is exactly what the public log proves.
                let text = mark.to_string();
                (text.clone(), text.clone(), text, mark.to_string(), row.prev)
            } else {
                let fixed = |text: &str| {
                    text.parse::<Fixed>().with_context(|| format!("a stored mark {text:?} does not parse"))
                };
                (
                    fixed(&row.open)?.to_string(),
                    fixed(&row.high)?.max(mark).to_string(),
                    fixed(&row.low)?.min(mark).to_string(),
                    mark.to_string(),
                    row.prev,
                )
            }
        }
    };
    let provenance = Provenance::LogDerived.as_str();
    let tag = tag(
        mac,
        market,
        resolution,
        bucket,
        &Row { open: &open, high: &high, low: &low, close: &close, provenance, prev },
    );
    let fresh = row.is_none();
    tx.execute(
        "INSERT OR REPLACE INTO marks (market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![i64::from(market), resolution as i64, bucket, open, high, low, close, provenance, prev, tag],
    )
    .with_context(|| format!("storing the {market} {resolution:?} bucket at {bucket}"))?;
    if fresh {
        relink_successor(mac, tx, market, resolution, bucket, &BTreeSet::new())?;
    }
    Ok(())
}

/// After a fresh bucket `bucket`, re-links the pre-existing row above it when the insert leaves
/// that link stale: the floor's NULL, or a gap the insert fills. Rows in `fresh`, inserted by the
/// same transaction, are skipped: they chained to the bucket as they went in. A link naming any
/// other bucket — including the insert itself, which a pre-existing row can only name if the
/// bucket existed before and something deleted it — is refused; the writer re-links only over
/// what its own inserts explain.
/// Error cases:
/// - the transaction fails, or the successor's row fails authentication
fn relink_successor(
    mac: &hmac::Hmac<sha2::Sha256>,
    tx: &Transaction,
    market: MarketId,
    resolution: Resolution,
    bucket: Millis,
    fresh: &BTreeSet<Millis>,
) -> anyhow::Result<()> {
    let succ = tx
        .query_row(SUCCESSOR, params![i64::from(market), resolution as i64, bucket], |row| {
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
        .with_context(|| format!("reading the {market} {resolution:?} chain above {bucket}"))?;
    let Some(succ) = succ else { return Ok(()) };
    if fresh.contains(&succ.bucket) {
        return Ok(());
    }
    verify_row(mac, market, resolution, &succ)?;
    match succ.prev {
        // The insert is the new floor below the row, or fills the gap its link named.
        None => retag(mac, tx, market, resolution, &succ, Some(bucket)),
        Some(p) if p < bucket => {
            // The link's old target must predate this transaction. A bucket this transaction
            // inserted cannot be the predecessor a pre-existing row names: naming it proves the
            // bucket existed when the row was written and something deleted it since, and
            // re-linking over its recreation would heal that deletion.
            ensure!(
                !fresh.contains(&p),
                "the {market} {resolution:?} chain is broken at {}: its link names the recreated bucket {p}",
                succ.bucket
            );
            let exists: i64 = tx
                .query_row(EXISTS, params![i64::from(market), resolution as i64, p], |row| row.get(0))
                .with_context(|| format!("reading the {market} {resolution:?} chain at {p}"))?;
            ensure!(
                exists == 1,
                "the {market} {resolution:?} chain is broken at {}: its link names the missing bucket {p}",
                succ.bucket
            );
            retag(mac, tx, market, resolution, &succ, Some(bucket))
        }
        // Any other link names a bucket that cannot sit between the insert and the successor it
        // found: either it names the insert itself — proving the bucket existed and was deleted —
        // or the chain was rewritten. Both refuse.
        Some(p) => bail!(
            "the {market} {resolution:?} chain is broken at {}: its link names {p} above the insert at {bucket}",
            succ.bucket
        ),
    }
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
        for bucket in fresh.iter().copied() {
            relink_successor(mac, tx, market, resolution, bucket, &fresh)?;
        }
    }
    Ok(inserted)
}
