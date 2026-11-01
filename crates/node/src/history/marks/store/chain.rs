//! The chained index over the marks cache: every bucket of one (market, resolution) pair names
//! the bucket below it (`prev_bucket`, NULL for the chain's floor), and every row's tag binds
//! that name, so a host that deletes, reorders, or rolls rows back breaks the chain the next
//! read walks. The writer maintains the invariant inside each transaction — snapshotting what
//! was there first, and refusing, never silently re-linking, when a predecessor reference no
//! longer resolves — because a vanished predecessor the writer did not itself delete is host
//! tampering that reads must keep seeing.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, bail, ensure};
use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};
use rusqlite::{OptionalExtension, Transaction, params};

use crate::history::marks::Resolution;
use crate::history::marks::store::{Row, Seed, SnapRow, retag, tag, verify_row};
use crate::history::marks::{Provenance, Retentions};

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
/// other vanished bucket is refused; the writer re-links only over what its own inserts explain.
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
        // Already linked over the insert (only a same-transaction row can be, and those are
        // skipped above): nothing to do.
        Some(p) if p == bucket => Ok(()),
        // The insert is the new floor below the row, or fills the gap its link named.
        None => retag(mac, tx, market, resolution, &succ, Some(bucket)),
        Some(p) if p < bucket => {
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
        // A link naming a bucket above the insert names a row that cannot exist below the
        // successor the query found: the chain was rewritten.
        Some(p) => bail!(
            "the {market} {resolution:?} chain is broken at {}: its link names {p} above the insert at {bucket}",
            succ.bucket
        ),
    }
}

/// Drops buckets older than their resolution's retention, counted back from `newest`, then
/// re-floors each market whose floor pruned away: the new floor's link chains from nothing.
/// Error cases:
/// - the transaction fails, or a floor row the prune exposed fails authentication
pub(crate) fn prune(
    mac: &hmac::Hmac<sha2::Sha256>,
    tx: &Transaction,
    retentions: &Retentions,
    newest: Millis,
) -> anyhow::Result<()> {
    for resolution in Resolution::ALL {
        let Some(retention) = retentions.of_resolution(resolution) else { continue };
        let horizon = newest.saturating_sub(retention);
        let removed = tx
            .execute("DELETE FROM marks WHERE resolution = ?1 AND bucket < ?2", params![resolution as i64, horizon])
            .with_context(|| format!("pruning {resolution:?} marks"))?;
        if removed == 0 {
            continue;
        }
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
            .query_map(params![resolution as i64], |row| {
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
            if floor.prev.is_some() {
                verify_row(mac, market, resolution, &floor)?;
                retag(mac, tx, market, resolution, &floor, None)?;
            }
        }
    }
    Ok(())
}

/// Deletes every log-derived bucket and re-chains what survives, so a recovery re-derivation
/// rebuilds exactly what the verified log proves. The snapshot the delete reasons over is taken
/// first: a surviving row whose link names a bucket this delete removed re-links; one whose link
/// names a bucket that vanished any other way refuses, never healing a host deletion.
/// Error cases:
/// - the transaction fails, a stored row fails authentication, or a link names a bucket the
///   delete did not remove
pub(crate) fn reset_log_derived(mac: &hmac::Hmac<sha2::Sha256>, tx: &Transaction) -> anyhow::Result<usize> {
    let snapshot = snapshot_all(mac, tx)?;
    let deleted: BTreeSet<(MarketId, Resolution, Millis)> = snapshot
        .iter()
        .flat_map(|((market, resolution), rows)| {
            rows.iter()
                .filter(|row| row.provenance == Provenance::LogDerived.as_str())
                .map(move |row| (*market, *resolution, row.bucket))
        })
        .collect();
    let removed = tx
        .execute("DELETE FROM marks WHERE provenance = ?1", [Provenance::LogDerived.as_str()])
        .context("resetting log-derived marks")?;
    for ((market, resolution), rows) in &snapshot {
        let mut pred = None;
        for row in rows {
            if deleted.contains(&(*market, *resolution, row.bucket)) {
                continue;
            }
            if row.prev != pred {
                ensure!(
                    row.prev.is_some_and(|p| deleted.contains(&(*market, *resolution, p))),
                    "the {market} {resolution:?} chain is broken at {}: its link names a bucket no delete removed",
                    row.bucket
                );
                retag(mac, tx, *market, *resolution, row, pred)?;
            }
            pred = Some(row.bucket);
        }
    }
    Ok(removed)
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

/// Every pair's rows, verified, in bucket order: what a whole-cache re-chain reasons over.
/// Error cases:
/// - the transaction fails, or a stored row fails authentication
fn snapshot_all(
    mac: &hmac::Hmac<sha2::Sha256>,
    tx: &Transaction,
) -> anyhow::Result<BTreeMap<(MarketId, Resolution), Vec<SnapRow>>> {
    let mut pairs: BTreeMap<(MarketId, Resolution), Vec<SnapRow>> = BTreeMap::new();
    let mut stmt = tx
        .prepare("SELECT market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag FROM marks ORDER BY market, resolution, bucket")
        .context("snapshotting the marks cache")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, MarketId>(0)?,
                row.get::<_, i64>(1)?,
                SnapRow {
                    bucket: row.get(2)?,
                    open: row.get(3)?,
                    high: row.get(4)?,
                    low: row.get(5)?,
                    close: row.get(6)?,
                    provenance: row.get(7)?,
                    prev: row.get(8)?,
                    tag: row.get(9)?,
                },
            ))
        })
        .context("snapshotting the marks cache")?;
    for row in rows {
        let (market, resolution, row) = row.context("snapshotting the marks cache")?;
        let resolution = Resolution::from_tag(u8::try_from(resolution).context("a stored resolution is out of range")?)
            .context("a stored resolution is out of range")?;
        verify_row(mac, market, resolution, &row)?;
        pairs.entry((market, resolution)).or_default().push(row);
    }
    Ok(pairs)
}
