//! The recovery re-derivation's reset: deleting every log-derived bucket and re-chaining what
//! survives, so the cache rebuilds exactly what the verified log proves. The snapshot the delete
//! reasons over is taken first: a surviving row whose link names a bucket this delete removed
//! re-links; one whose link names a bucket that vanished any other way refuses, never healing a
//! host deletion. Every pair is validated against its committed anchor before anything moves.
//! The survivors are re-bound end to end — each one's link and recorded predecessor tag made the
//! row actually below it — so no earlier pin outlives the reset.
//! Each call takes one verified snapshot of the whole cache, linear in its size — the validation
//! must precede the delete, and the cache is bounded by its retentions — and walks it once for
//! both the links and the delete. Startup's validation and the reset each read the file afresh:
//! nothing carries a snapshot between them, because the host can rewrite the file in between.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, ensure};
use rusqlite::Transaction;

use engine_core::types::{MarketId, Millis};

use crate::history::marks::Provenance;
use crate::history::marks::Resolution;
use crate::history::marks::store::chain::{ChainUpdates, anchor_updates, check_anchors, retag, verify_row};
use crate::history::marks::store::{ChainAnchors, SnapRow};

/// Validates every pair's chain as the file holds it: every row authenticates, and every link
/// names the row actually below it. What a delete later removes is validated through the same
/// walk, so a hole the file already carries is refused here, before any mutation.
/// Error cases:
/// - a stored row fails authentication, or a link names a bucket that is already missing
pub(crate) fn validate_chains(mac: &hmac::Hmac<sha2::Sha256>, tx: &Transaction) -> anyhow::Result<()> {
    check_links(&snapshot_all(mac, tx)?)
}

/// Requires every link of an already verified snapshot to name the row actually below it.
/// Error cases:
/// - a link names a bucket that is already missing
fn check_links(snapshot: &Snapshot) -> anyhow::Result<()> {
    for ((market, resolution), rows) in snapshot {
        let mut actual = None;
        for row in rows {
            ensure!(
                row.prev == actual,
                "the {market} {resolution:?} chain is broken at {}: its link names a bucket that is already missing",
                row.bucket
            );
            actual = Some(row.bucket);
        }
    }
    Ok(())
}

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
    next: &hmac::Hmac<sha2::Sha256>,
    tx: &Transaction,
    prior: &ChainAnchors,
) -> anyhow::Result<(usize, ChainUpdates)> {
    let snapshot = snapshot_all(mac, tx)?;
    check_links(&snapshot)?;
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
    // check_links covered every link, including links on rows this delete removes: the delete
    // also removes every reference to them, and a row whose own link names a missing bucket
    // would let the delete launder a host deletion into a clean re-chain.
    rebind(next, tx, &snapshot, &deleted)?;
    let updates = anchor_updates(tx, &pairs, &Default::default(), true)?;
    Ok((removed, updates))
}

/// Re-tags every row of `snapshot` that `deleted` does not name under `mac`, the new run key: its
/// link made the survivor actually below it, where its predecessor is a row the delete removed,
/// and the tag it records made the tag that survivor carries now, so no earlier pin and no earlier
/// version outlives it.
/// Error cases:
/// - an update fails
fn rebind(
    mac: &hmac::Hmac<sha2::Sha256>,
    tx: &Transaction,
    snapshot: &Snapshot,
    deleted: &BTreeSet<(MarketId, Resolution, Millis)>,
) -> anyhow::Result<()> {
    for ((market, resolution), rows) in snapshot {
        let mut pred: Option<(Millis, [u8; 16])> = None;
        for row in rows {
            if deleted.contains(&(*market, *resolution, row.bucket)) {
                continue;
            }
            let tag = retag(mac, tx, *market, *resolution, row, pred)?;
            pred = Some((row.bucket, tag));
        }
    }
    Ok(())
}

/// Every pair's rows, verified, in bucket order: what a whole-cache re-chain reasons over.
type Snapshot = BTreeMap<(MarketId, Resolution), Vec<SnapRow>>;

/// Reads the whole cache as a `Snapshot`.
/// Error cases:
/// - the transaction fails, or a stored row fails authentication
fn snapshot_all(mac: &hmac::Hmac<sha2::Sha256>, tx: &Transaction) -> anyhow::Result<Snapshot> {
    let mut pairs = Snapshot::new();
    let mut stmt = tx
        .prepare("SELECT market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag, prev_tag FROM marks ORDER BY market, resolution, bucket")
        .context("snapshotting the marks cache")?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, MarketId>(0)?, row.get::<_, i64>(1)?, SnapRow::from_row(row, 2)?)))
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
