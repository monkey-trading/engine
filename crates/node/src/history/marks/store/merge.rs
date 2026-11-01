//! Merging a log mark into its bucket: the live write path. A fresh bucket takes the chain
//! link naming the bucket below it and repairs the floor or gap link above it; a row the host
//! altered — to other prices, another provenance, an unparseable value, or a broken chain link —
//! fails the write, which then changes nothing.

use std::collections::BTreeSet;

use anyhow::{Context, ensure};
use rusqlite::{OptionalExtension, Transaction, params};

use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};

use crate::history::marks::Provenance;
use crate::history::marks::Resolution;
use crate::history::marks::store::SnapRow;
use crate::history::marks::store::auth::{Row, tag};
use crate::history::marks::store::chain::{PREDECESSOR, ROW_AT, SUCCESSOR, retag, verify_row};

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

/// The greatest pre-existing bucket below `bucket`: the walk steps past every bucket this
/// transaction inserted, so the caller compares a link against the chain as it stood before
/// the transaction.
/// Error cases:
/// - the predecessor query fails
fn pred_below(
    tx: &Transaction,
    market: MarketId,
    resolution: Resolution,
    bucket: Millis,
) -> anyhow::Result<Option<Millis>> {
    tx.query_row(PREDECESSOR, params![i64::from(market), resolution as i64, bucket], |row| row.get(0))
        .with_context(|| format!("reading the {market} {resolution:?} chain below {bucket}"))
}

/// After a fresh bucket `bucket`, re-links the pre-existing row above it when the insert leaves
/// that link stale. The successor's authenticated link must name the greatest pre-existing
/// bucket below it — this transaction's fresh run included, walked past — or the insert is
/// filling the very gap, or the floor position, that link records: any other shape, including
/// a NULL link with pre-existing rows below, or a link naming the insert itself, is a restored
/// or rewritten row the writer refuses to build on, never heals. Rows in `fresh`, inserted by
/// the same transaction, are skipped: they chained to the bucket as they went in.
/// Error cases:
/// - the transaction fails, or the successor's row fails authentication
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
    let mut below = pred_below(tx, market, resolution, succ.bucket)?;
    while below.is_some_and(|b| fresh.contains(&b)) {
        below = pred_below(tx, market, resolution, below.expect("a bucket below"))?;
    }
    ensure!(
        succ.prev == below,
        "the {market} {resolution:?} chain is broken at {}: its link does not name the bucket below it",
        succ.bucket
    );
    retag(mac, tx, market, resolution, &succ, Some(bucket))
}
