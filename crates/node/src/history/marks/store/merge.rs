//! Merging a log mark into its bucket: the live write path. A fresh bucket takes the chain
//! link naming the bucket below it and repairs the floor or gap link above it; a row the host
//! altered — to other prices, another provenance, an unparseable value, or a broken chain link —
//! fails the write, which then changes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context, ensure};
use rusqlite::{OptionalExtension, Statement, Transaction, params};

use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};

use crate::history::marks::Provenance;
use crate::history::marks::Resolution;
use crate::history::marks::store::ChainAnchors;
use crate::history::marks::store::SnapRow;
use crate::history::marks::store::auth::{Interior, Rewrites, Row, tag};
use crate::history::marks::store::chain::{PREDECESSOR, ROW_AT, SUCCESSOR, ensure_current, retag, rewrote, verify_row};

/// The row store a merge writes, shared with nothing else so the statement is the merge's alone.
const INSERT: &str =
    "INSERT OR REPLACE INTO marks (market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag, prev_tag)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";

/// The statements one write prepares once and every sample's merge reuses: the row a sample
/// lands in, the chain link below it, the store that writes it, and the row above a fresh
/// bucket, with what the write rewrote in place: the head and pins each pair stood at, so a merge
/// into a lower bucket is known to sit under a row, and the buckets rewritten. Dropped before the
/// write commits.
pub(crate) struct MergeStmts<'tx> {
    row_at: Statement<'tx>,
    predecessor: Statement<'tx>,
    insert: Statement<'tx>,
    successor: Statement<'tx>,
    pairs: BTreeMap<(MarketId, Resolution), PairState>,
    rewrites: Rewrites,
}

/// A pair as the write found it: where its head stood and the rows pinned in it.
struct PairState {
    head: Millis,
    pins: Arc<Interior>,
}

impl<'tx> MergeStmts<'tx> {
    /// Prepares the four statements on `tx`, for `pairs` as `prior` committed them.
    /// Error cases:
    /// - a statement does not prepare
    pub(crate) fn prepare(
        tx: &'tx Transaction,
        prior: &ChainAnchors,
        pairs: &BTreeSet<(MarketId, Resolution)>,
    ) -> anyhow::Result<Self> {
        let prepare = |sql| tx.prepare(sql).context("preparing a marks merge");
        Ok(Self {
            row_at: prepare(ROW_AT)?,
            predecessor: prepare(PREDECESSOR)?,
            insert: prepare(INSERT)?,
            successor: prepare(SUCCESSOR)?,
            pairs: pairs
                .iter()
                .filter_map(|&pair| {
                    let head = prior.get(&pair)?.head.bucket;
                    Some((pair, PairState { head, pins: prior.interior(pair) }))
                })
                .collect(),
            rewrites: Rewrites::new(),
        })
    }

    /// Requires the stored row at `bucket` with `tag` to be the version last written for it
    /// (see `ensure_current`), as the write began or as this write left it.
    /// Error cases:
    /// - the row is not that version
    pub(crate) fn ensure_current(
        &self,
        mac: &hmac::Hmac<sha2::Sha256>,
        tx: &Transaction,
        market: MarketId,
        resolution: Resolution,
        bucket: Millis,
        tag: [u8; 16],
    ) -> anyhow::Result<()> {
        let pair = (market, resolution);
        let pins = self.pairs.get(&pair).map(|state| state.pins.as_ref());
        ensure_current(mac, tx, pins, self.rewrites.get(&pair), pair, bucket, tag)
    }

    /// The bucket below `bucket` and its tag, if any.
    /// Error cases:
    /// - the predecessor query fails
    pub(crate) fn predecessor(
        &mut self,
        market: MarketId,
        resolution: Resolution,
        bucket: Millis,
    ) -> anyhow::Result<Option<(Millis, [u8; 16])>> {
        self.predecessor
            .query_row(params![i64::from(market), resolution as i64, bucket], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()
            .with_context(|| format!("reading the {market} {resolution:?} chain below {bucket}"))
    }

    /// The buckets this write rewrote in place.
    pub(crate) fn into_rewrites(self) -> Rewrites {
        self.rewrites
    }
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
    stmts: &mut MergeStmts<'_>,
    market: MarketId,
    resolution: Resolution,
    bucket: Millis,
    mark: Fixed,
) -> anyhow::Result<()> {
    let row = stmts
        .row_at
        .query_row(params![i64::from(market), resolution as i64, bucket], |row| {
            Ok(SnapRow {
                bucket,
                open: row.get(0)?,
                high: row.get(1)?,
                low: row.get(2)?,
                close: row.get(3)?,
                provenance: row.get(4)?,
                prev: row.get(5)?,
                tag: row.get(6)?,
                prev_tag: row.get(7)?,
            })
        })
        .optional()
        .with_context(|| format!("reading the {market} {resolution:?} bucket at {bucket}"))?;
    let pred = stmts.predecessor(market, resolution, bucket)?;
    let (pred_bucket, pred_tag) = (pred.map(|pred| pred.0), pred.map(|pred| pred.1));
    // Only a write below the head builds on a row with another above it; the head itself was
    // checked against its committed anchor when the write began.
    let below_head = stmts.pairs.get(&(market, resolution)).is_some_and(|state| bucket < state.head);
    let (open, high, low, close, prev, prev_tag) = match &row {
        None => {
            let text = mark.to_string();
            (text.clone(), text.clone(), text.clone(), text, pred_bucket, pred_tag)
        }
        Some(row) => {
            // A row whose tag does not match its stored fields, or whose link names a bucket that
            // is not the one actually below it, was rewritten or undermined; refuse to build on
            // it, so host edits never reach a merge or a signed answer.
            verify_row(mac, market, resolution, row)
                .with_context(|| format!("reading the {market} {resolution:?} bucket at {bucket}"))?;
            ensure!(
                row.prev == pred_bucket,
                "the {market} {resolution:?} chain is broken at {bucket}: its link names a bucket that is no longer below it"
            );
            if below_head {
                stmts.ensure_current(mac, tx, market, resolution, bucket, row.tag)?;
            }
            if row.provenance != Provenance::LogDerived.as_str() {
                // A provider-seeded bucket the log now speaks for is rebuilt from the log mark
                // alone, so a log-derived bucket is exactly what the public log proves.
                let text = mark.to_string();
                (text.clone(), text.clone(), text, mark.to_string(), row.prev, row.prev_tag)
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
                    row.prev_tag,
                )
            }
        }
    };
    if row.is_none()
        && below_head
        && let Some((below, below_tag)) = pred
    {
        // A gap fill binds its predecessor's tag: refuse to bind a restored older version.
        stmts.ensure_current(mac, tx, market, resolution, below, below_tag)?;
    }
    let provenance = Provenance::LogDerived.as_str();
    let tag = tag(
        mac,
        market,
        resolution,
        bucket,
        &Row { open: &open, high: &high, low: &low, close: &close, provenance, prev, prev_tag },
    );
    let fresh = row.is_none();
    stmts
        .insert
        .execute(params![
            i64::from(market),
            resolution as i64,
            bucket,
            open,
            high,
            low,
            close,
            provenance,
            prev,
            tag,
            prev_tag
        ])
        .with_context(|| format!("storing the {market} {resolution:?} bucket at {bucket}"))?;
    let state =
        stmts.pairs.entry((market, resolution)).or_insert_with(|| PairState { head: bucket, pins: Arc::default() });
    if fresh && bucket > state.head {
        state.head = bucket;
    } else if !fresh && (bucket < state.head || state.pins.contains_key(&bucket)) {
        // A row with one above it that changed, or a pinned head that did: the row above bound
        // its earlier tag, and the pin is its tag now.
        rewrote(&mut stmts.rewrites, market, resolution, bucket);
    }
    if fresh {
        relink_successor(mac, tx, stmts, market, resolution, bucket, &BTreeSet::from([bucket]))?;
    }
    Ok(())
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
    stmts: &mut MergeStmts<'_>,
    market: MarketId,
    resolution: Resolution,
    bucket: Millis,
    fresh: &BTreeSet<Millis>,
) -> anyhow::Result<()> {
    let succ = stmts
        .successor
        .query_row(params![i64::from(market), resolution as i64, bucket], |row| {
            Ok(SnapRow {
                bucket: row.get(0)?,
                open: row.get(1)?,
                high: row.get(2)?,
                low: row.get(3)?,
                close: row.get(4)?,
                provenance: row.get(5)?,
                prev: row.get(6)?,
                tag: row.get(7)?,
                prev_tag: row.get(8)?,
            })
        })
        .optional()
        .with_context(|| format!("reading the {market} {resolution:?} chain above {bucket}"))?;
    let Some(succ) = succ else { return Ok(()) };
    if fresh.contains(&succ.bucket) {
        return Ok(());
    }
    verify_row(mac, market, resolution, &succ)?;
    let mut below = stmts.predecessor(market, resolution, succ.bucket)?;
    while let Some((b, _)) = below.filter(|(b, _)| fresh.contains(b)) {
        below = stmts.predecessor(market, resolution, b)?;
    }
    ensure!(
        succ.prev == below.map(|below| below.0),
        "the {market} {resolution:?} chain is broken at {}: its link does not name the bucket below it",
        succ.bucket
    );
    stmts.ensure_current(mac, tx, market, resolution, succ.bucket, succ.tag)?;
    // The row now below the successor is the insert itself, as stored.
    let insert_tag: [u8; 16] = tx
        .query_row(
            "SELECT tag FROM marks WHERE market = ?1 AND resolution = ?2 AND bucket = ?3",
            params![i64::from(market), resolution as i64, bucket],
            |row| row.get(0),
        )
        .with_context(|| format!("reading the {market} {resolution:?} bucket at {bucket}"))?;
    retag(mac, tx, market, resolution, &succ, Some((bucket, insert_tag)))?;
    // The row above the successor bound the tag it carried until now.
    rewrote(&mut stmts.rewrites, market, resolution, succ.bucket);
    Ok(())
}
