//! The recovery re-derivation's reset: deleting every log-derived bucket and re-chaining what
//! survives, so the cache rebuilds exactly what the verified log proves. The snapshot the delete
//! reasons over is taken first: a surviving row whose link names a bucket this delete removed
//! re-links; one whose link names a bucket that vanished any other way refuses, never healing a
//! host deletion. Every pair is validated against its committed anchor before anything moves.
//! The price is one verified snapshot of the whole cache per boot, linear in its size — the
//! validation must precede the delete, and the cache is bounded by its retentions.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, ensure};
use rusqlite::Transaction;

use engine_core::types::{MarketId, Millis};

use crate::history::marks::Provenance;
use crate::history::marks::Resolution;
use crate::history::marks::store::chain::{ChainUpdates, anchor_updates, check_anchors, retag, verify_row};
use crate::history::marks::store::{ChainAnchors, SnapRow};

/// Deletes every log-derived bucket and re-chains what survives, so a recovery re-derivation
/// rebuilds exactly what the verified log proves. The snapshot the delete reasons over is taken
/// first: a surviving row whose link names a bucket this delete removed re-links; one whose link
/// names a bucket that vanished any other way refuses, never healing a host deletion. Every pair
/// is validated against its committed anchor before anything moves. Returns how many rows were
/// removed, with the anchor updates to publish with the commit.
/// Error cases:
/// - the transaction fails, a stored row fails authentication, a link names a bucket this delete
///   did not remove, or a pair no longer matches its committed anchor
pub(crate) fn reset_log_derived(
    mac: &hmac::Hmac<sha2::Sha256>,
    tx: &Transaction,
    prior: &ChainAnchors,
) -> anyhow::Result<(usize, ChainUpdates)> {
    let snapshot = snapshot_all(mac, tx)?;
    let pairs: BTreeSet<(MarketId, Resolution)> = snapshot.keys().chain(prior.keys()).copied().collect();
    check_anchors(tx, prior, &pairs)?;
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
        // Every link must name the row actually below it in the snapshot, including links on
        // rows this delete removes: the delete also removes every reference to them, and a row
        // whose own link already names a missing bucket would let the delete launder a host
        // deletion into a clean re-chain.
        let mut actual = None;
        for row in rows {
            ensure!(
                row.prev == actual,
                "the {market} {resolution:?} chain is broken at {}: its link names a bucket that is already missing",
                row.bucket
            );
            actual = Some(row.bucket);
        }
        let mut pred = None;
        for row in rows {
            if deleted.contains(&(*market, *resolution, row.bucket)) {
                continue;
            }
            if row.prev != pred {
                // The only re-tag the validated snapshot allows: the row's predecessor is a row
                // this delete removed, and the survivors close over it.
                retag(mac, tx, *market, *resolution, row, pred)?;
            }
            pred = Some(row.bucket);
        }
    }
    let updates = anchor_updates(tx, &pairs)?;
    Ok((removed, updates))
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
