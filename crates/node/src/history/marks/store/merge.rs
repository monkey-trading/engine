//! Merging a log mark into its bucket: the live write path. A fresh bucket takes the chain
//! link naming the bucket below it and repairs the floor or gap link above it; a row the host
//! altered — to other prices, another provenance, an unparseable value, or a broken chain link —
//! fails the write, which then changes nothing.

use std::collections::BTreeSet;

use anyhow::{Context, bail, ensure};
use rusqlite::{OptionalExtension, Transaction, params};

use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};

use crate::history::marks::Provenance;
use crate::history::marks::Resolution;
use crate::history::marks::store::SnapRow;
use crate::history::marks::store::auth::{Row, tag};
use crate::history::marks::store::chain::{EXISTS, PREDECESSOR, ROW_AT, SUCCESSOR, retag, verify_row};

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
        relink_successor(mac, tx, market, resolution, bucket, &BTreeSet::from([bucket]))?;
    }
    Ok(())
}

/// After a fresh bucket `bucket`, re-links the pre-existing row above it when the insert leaves
/// that link stale: the floor's NULL, or a gap the insert fills. Rows in `fresh`, inserted by the
/// same transaction, are skipped: they chained to the bucket as they went in. A NULL link is the
/// floor's only when nothing pre-existing sits below the insert either (`pred` is `None` or
/// fresh); a link naming any other bucket — including the insert itself, which a pre-existing row
/// can only name if the bucket existed before and something deleted it — is refused. The writer
/// re-links only over what its own inserts explain.
/// Error cases:
/// - the transaction fails, or the successor's row fails authentication
fn pred_below(
    tx: &Transaction,
    market: MarketId,
    resolution: Resolution,
    bucket: Millis,
) -> anyhow::Result<Option<Millis>> {
    tx.query_row(PREDECESSOR, params![i64::from(market), resolution as i64, bucket], |row| row.get(0))
        .with_context(|| format!("reading the {market} {resolution:?} chain below {bucket}"))
}

pub(crate) fn relink_successor(
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
        // A NULL link names the floor: genuine only when no pre-existing row sits anywhere
        // below the successor — this transaction's fresh run included, so walk past it to the
        // nearest pre-existing bucket. A NULL link with a pre-existing row below is a restored
        // snapshot row refusing reads.
        None => {
            let mut below = pred_below(tx, market, resolution, succ.bucket)?;
            while below.is_some_and(|b| fresh.contains(&b)) {
                below = pred_below(tx, market, resolution, below.expect("a bucket below"))?;
            }
            ensure!(
                below.is_none(),
                "the {market} {resolution:?} chain is broken at {}: its floor link does not name the bucket below it",
                succ.bucket
            );
            retag(mac, tx, market, resolution, &succ, Some(bucket))
        }
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
