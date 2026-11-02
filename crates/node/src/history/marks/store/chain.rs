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
use rusqlite::{Connection, OptionalExtension, Transaction, named_params, params};

use crate::history::marks::Resolution;
use crate::history::marks::store::auth::{Interior, PairCommit, Rewrites, Row, tag};
use crate::history::marks::store::merge;
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
    let expected = tag(mac, market, resolution, row.bucket, &row.fields());
    ensure!(row.tag == expected, "the {market} {resolution:?} bucket at {} fails authentication", row.bucket);
    Ok(())
}

/// Re-links `row` to `prev` (the bucket below it, with the tag that row carries), keeping its
/// content: updates the chain link and the tag that binds it, in the transaction that reasons over
/// the chain. Returns the row's new tag.
/// Error cases:
/// - the update fails
pub(crate) fn retag(
    mac: &hmac::Hmac<sha2::Sha256>,
    tx: &rusqlite::Transaction,
    market: MarketId,
    resolution: Resolution,
    row: &SnapRow,
    below: Option<(Millis, [u8; 16])>,
) -> anyhow::Result<[u8; 16]> {
    let (prev, prev_tag) = (below.map(|below| below.0), below.map(|below| below.1));
    let fields = Row { prev, prev_tag, ..row.fields() };
    let tag = tag(mac, market, resolution, row.bucket, &fields);
    tx.execute(
        "UPDATE marks SET prev_bucket = ?4, tag = ?5, prev_tag = ?6
         WHERE market = ?1 AND resolution = ?2 AND bucket = ?3",
        params![i64::from(market), resolution as i64, row.bucket, prev, tag, prev_tag],
    )
    .with_context(|| format!("re-linking the {market} {resolution:?} bucket at {}", row.bucket))?;
    Ok(tag)
}

/// The row directly above `bucket`: its bucket, the link it holds, and the tag it recorded for the
/// row below.
const BINDING: &str = "SELECT bucket, prev_bucket, prev_tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket > :bucket ORDER BY bucket ASC LIMIT 1";

/// Requires the stored row at `bucket` with `tag` to be the version this node last wrote for it
/// before a write builds on it, re-tags it, or binds its tag into a new row: the host can put an
/// older authentic version back, and a write that kept its content under a new tag would bless it.
/// A row this transaction rewrote (`written`) is current by construction; a pinned row must carry
/// its pin; any other row with a row above it must be the tag that row recorded; and a row with
/// nothing above it is the head, which the committed head anchor already checked.
/// Error cases:
/// - the row is not the pinned version, or not the one the row above it recorded
pub(crate) fn ensure_current(
    tx: &Transaction,
    pins: Option<&Interior>,
    written: Option<&BTreeSet<Millis>>,
    market: MarketId,
    resolution: Resolution,
    bucket: Millis,
    tag: [u8; 16],
) -> anyhow::Result<()> {
    if written.is_some_and(|written| written.contains(&bucket)) {
        return Ok(());
    }
    if let Some(pinned) = pins.and_then(|pins| pins.get(&bucket)) {
        ensure!(*pinned == tag, "the {market} {resolution:?} bucket at {bucket} is not the version committed for it");
        return Ok(());
    }
    let above: Option<(Millis, Option<Millis>, Option<[u8; 16]>)> = tx
        .query_row(
            BINDING,
            named_params! {":market": i64::from(market), ":resolution": resolution as i64, ":bucket": bucket},
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .with_context(|| format!("reading the {market} {resolution:?} chain above {bucket}"))?;
    if let Some((above, prev, prev_tag)) = above {
        ensure!(
            prev == Some(bucket),
            "the {market} {resolution:?} chain is broken at {above}: its link does not name the bucket below it"
        );
        ensure!(
            prev_tag == Some(tag),
            "the {market} {resolution:?} bucket at {bucket} is not the version the row above it recorded"
        );
    }
    Ok(())
}

/// Notes that `bucket` was rewritten in place, so `anchor_updates` pins its tag.
pub(crate) fn rewrote(rewrites: &mut Rewrites, market: MarketId, resolution: Resolution, bucket: Millis) {
    rewrites.entry((market, resolution)).or_default().insert(bucket);
}

/// One stored row of one pair, for the merge's read of the bucket a sample lands in.
pub(crate) const ROW_AT: &str = "SELECT open, high, low, close, provenance, prev_bucket, tag, prev_tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket = :bucket";
/// The greatest bucket below `bucket` and its tag, the chain link a stored or new row there must
/// name and the tag a new row binds.
pub(crate) const PREDECESSOR: &str = "SELECT bucket, tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket < :bucket ORDER BY bucket DESC LIMIT 1";
/// The least bucket above `bucket`, whose link a fresh insert can leave stale: only the floor's
/// NULL or a gap the insert fills may change, and only to the insert itself.
pub(crate) const SUCCESSOR: &str =
    "SELECT bucket, open, high, low, close, provenance, prev_bucket, tag, prev_tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket > :bucket ORDER BY bucket ASC LIMIT 1";
/// Whether a bucket holds a row, for a successor link that names a bucket below the insert.
pub(crate) const EXISTS: &str =
    "SELECT EXISTS(SELECT 1 FROM marks WHERE market = :market AND resolution = :resolution AND bucket = :bucket)";
/// One pair's whole floor row, which a prune checks against the last bucket it deleted.
const FLOOR_ROW: &str = "SELECT bucket, open, high, low, close, provenance, prev_bucket, tag, prev_tag FROM marks
    WHERE market = ?1 AND resolution = ?2 ORDER BY bucket ASC LIMIT 1";
/// One pair's floor row, half of its committed anchor.
const FLOOR: &str = "SELECT bucket, tag FROM marks
    WHERE market = :market AND resolution = :resolution ORDER BY bucket ASC LIMIT 1";
/// One pair's head row, half of its committed anchor.
const HEAD: &str = "SELECT bucket, tag FROM marks
    WHERE market = :market AND resolution = :resolution ORDER BY bucket DESC LIMIT 1";

/// The updates one write publishes with its commit: `Some` for a pair's freshly committed floor
/// and head and the interior rows it rewrote, `None` for a pair whose every bucket the write
/// removed.
pub(crate) type ChainUpdates = BTreeMap<(MarketId, Resolution), Option<PairCommit>>;

/// One pair's planned deletion: its horizon and the rows below it, in bucket order.
struct PairPrune {
    market: MarketId,
    resolution: Resolution,
    horizon: Millis,
    rows: Vec<SnapRow>,
}

/// The buckets one prune will delete, per pair: planned before any mutation so the pairs a write
/// touches, and validates, are known up front.
pub(crate) struct PrunePlan {
    pairs: Vec<PairPrune>,
}

/// The rows of one pair below its horizon, a primary-key prefix search.
pub(crate) const EXPIRING: &str =
    "SELECT bucket, open, high, low, close, provenance, prev_bucket, tag, prev_tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket < :horizon ORDER BY bucket";
/// The delete of the rows `EXPIRING` plans, the same primary-key prefix.
pub(crate) const DELETE_EXPIRED: &str =
    "DELETE FROM marks WHERE market = :market AND resolution = :resolution AND bucket < :horizon";

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

/// Derives the updates for the touched pairs from the transaction's own post-write view, so the
/// committed anchors describe exactly what the transaction wrote, never unvalidated disk. Every
/// bucket the write rewrote is pinned at its tag. `rebound` marks a write that re-bound every row of the pairs, so no earlier pin survives.
/// Error cases:
/// - a pair's floor, head, or rewritten row cannot be read
pub(crate) fn anchor_updates(
    tx: &Transaction,
    pairs: &BTreeSet<(MarketId, Resolution)>,
    rewrites: &Rewrites,
    rebound: bool,
) -> anyhow::Result<ChainUpdates> {
    let mut updates = ChainUpdates::new();
    for &(market, resolution) in pairs {
        let Some(anchor) = pair_anchor(tx, market, resolution)? else {
            updates.insert((market, resolution), None);
            continue;
        };
        let mut rewritten = Interior::new();
        for &bucket in rewrites.get(&(market, resolution)).into_iter().flatten() {
            let tag: Option<[u8; 16]> = tx
                .query_row(
                    "SELECT tag FROM marks WHERE market = ?1 AND resolution = ?2 AND bucket = ?3",
                    params![i64::from(market), resolution as i64, bucket],
                    |row| row.get(0),
                )
                .optional()
                .with_context(|| format!("reading the rewritten {market} {resolution:?} bucket at {bucket}"))?;
            rewritten.extend(tag.map(|tag| (bucket, tag)));
        }
        updates.insert((market, resolution), Some(PairCommit { anchor, rewritten, rebound }));
    }
    Ok(updates)
}

/// The pairs whose committed floor lies below their resolution's horizon, counted back from
/// `newest`: besides the pairs a write itself touches, the only pairs with anything to prune,
/// because the committed floor is the lowest bucket the pair holds. Read from the anchors, so
/// finding them costs no query.
pub(crate) fn expiring_pairs(
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

/// Plans the buckets `pairs` will prune, counted back from `newest`: which rows die is decided
/// before anything mutates. `pairs` must hold every pair the write touched and every pair
/// `expiring_pairs` named; a pair outside it is not examined. Each pair costs one primary-key
/// prefix search, never a scan of its resolution. Planning reads only; `apply_prune` does the
/// deleting.
/// Error cases:
/// - the plan cannot be read
pub(crate) fn plan_prune(
    tx: &Transaction,
    retentions: &Retentions,
    newest: Millis,
    pairs: &BTreeSet<(MarketId, Resolution)>,
) -> anyhow::Result<PrunePlan> {
    let mut stmt = tx.prepare(EXPIRING).context("planning the prune")?;
    let mut planned = Vec::new();
    for &(market, resolution) in pairs {
        let Some(retention) = retentions.of_resolution(resolution) else { continue };
        let horizon = newest.saturating_sub(retention);
        let rows = stmt
            .query_map(
                named_params! {":market": i64::from(market), ":resolution": resolution as i64, ":horizon": horizon},
                |row| {
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
                },
            )
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .with_context(|| format!("planning the {market} {resolution:?} prune"))?;
        if !rows.is_empty() {
            planned.push(PairPrune { market, resolution, horizon, rows });
        }
    }
    Ok(PrunePlan { pairs: planned })
}

/// Deletes the planned buckets and re-floors each pair whose floor pruned away. The expiring
/// rows are a pair's prefix, and the delete removes them and every reference to them: each must
/// authenticate and name the row actually below it, or the delete would erase the last record of
/// a host deletion — including a record the host edited into a row that no longer verifies. A
/// surviving floor's link may be cleared only when it names a bucket this delete removed; a link
/// naming any other missing bucket is a host deletion this write refuses, never heals.
/// Error cases:
/// - the delete fails, an expiring or floor row fails authentication, a link names a bucket that
///   is already missing, a floor link names a bucket this prune did not delete, or the floor is
///   not the version last written for it
pub(crate) fn apply_prune(
    mac: &hmac::Hmac<sha2::Sha256>,
    tx: &Transaction,
    plan: &PrunePlan,
    rewrites: &mut Rewrites,
    prior: &ChainAnchors,
) -> anyhow::Result<()> {
    for PairPrune { market, resolution, horizon, rows } in &plan.pairs {
        let (market, resolution) = (*market, *resolution);
        let mut previous = None;
        for row in rows {
            // Authenticate before trusting the link: an edited row fails here, not by healing.
            verify_row(mac, market, resolution, row)?;
            ensure!(
                row.prev == previous,
                "the {market} {resolution:?} chain is broken at {}: its link names a bucket that is already missing",
                row.bucket
            );
            previous = Some(row.bucket);
        }
        let last = previous.expect("a planned pair has rows");
        tx.execute(
            DELETE_EXPIRED,
            named_params! {":market": i64::from(market), ":resolution": resolution as i64, ":horizon": horizon},
        )
        .with_context(|| format!("pruning {market} {resolution:?} marks"))?;
        // A pair that had rows below the horizon must survive at a floor whose link names the
        // last deleted bucket: every other survivor's predecessor is at or above the horizon and
        // still there. A NULL link or a link skipping that bucket is a restored snapshot row,
        // refused, not published as the new floor.
        let floor: Option<SnapRow> = tx
            .query_row(FLOOR_ROW, params![i64::from(market), resolution as i64], |row| {
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
            .with_context(|| format!("reading the pruned {resolution:?} floor of {market}"))?;
        let Some(floor) = floor else { continue };
        verify_row(mac, market, resolution, &floor)?;
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
        let pair = (market, resolution);
        let pins = prior.interior(pair);
        ensure_current(tx, Some(pins.as_ref()), rewrites.get(&pair), market, resolution, floor.bucket, floor.tag)?;
        retag(mac, tx, market, resolution, &floor, None)?;
        rewrote(rewrites, market, resolution, floor.bucket);
    }
    Ok(())
}

/// Seeds `rows`, provider candles turned into buckets, keeping every row already there: seeded
/// history never overwrites log-derived marks or an earlier seed. Inserts run oldest first, each
/// naming the bucket actually below it, and only the pre-existing row above an insert can need
/// re-linking — to the insert itself, never over a bucket that vanished some other way. Returns
/// how many rows were added, with the buckets it rewrote. `prior` holds the pins an insert's
/// predecessor and successor are checked against.
/// Error cases:
/// - the transaction fails, a stored row fails authentication, a link names a bucket this
///   seed did not insert, or a row it builds on is not the version last written for it
pub(crate) fn seed(
    mac: &hmac::Hmac<sha2::Sha256>,
    tx: &Transaction,
    rows: &[Seed],
    prior: &ChainAnchors,
) -> anyhow::Result<(usize, Rewrites)> {
    let mut by_pair: BTreeMap<(MarketId, Resolution), Vec<&Seed>> = BTreeMap::new();
    for row in rows {
        by_pair.entry((row.market, row.resolution)).or_default().push(row);
    }
    let mut inserted = 0;
    let pairs: BTreeSet<_> = by_pair.keys().copied().collect();
    let mut stmts = merge::MergeStmts::prepare(tx, prior, &pairs)?;
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
            let pred = stmts.predecessor(market, resolution, seed.ts)?;
            // The new row binds its predecessor's tag: refuse to bind a restored older version.
            if let Some((below, below_tag)) = pred.filter(|(below, _)| !fresh.contains(below)) {
                stmts.ensure_current(tx, market, resolution, below, below_tag)?;
            }
            let provenance = Provenance::SeededFromProvider.as_str();
            let (open, high, low, close) =
                (seed.open.to_string(), seed.high.to_string(), seed.low.to_string(), seed.close.to_string());
            let tag = tag(
                mac,
                market,
                resolution,
                seed.ts,
                &Row {
                    open: &open,
                    high: &high,
                    low: &low,
                    close: &close,
                    provenance,
                    prev: pred.map(|pred| pred.0),
                    prev_tag: pred.map(|pred| pred.1),
                },
            );
            inserted += tx
                .execute(
                    "INSERT INTO marks (market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag, prev_tag)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        i64::from(market),
                        resolution as i64,
                        seed.ts,
                        open,
                        high,
                        low,
                        close,
                        provenance,
                        pred.map(|pred| pred.0),
                        tag,
                        pred.map(|pred| pred.1)
                    ],
                )
                .with_context(|| format!("seeding {market} {resolution:?} at {}", seed.ts))?;
            fresh.insert(seed.ts);
        }
        // Only the pre-existing row directly above an insert can have a stale link: the rows
        // this seed inserted chained to each other as they went in.
        for bucket in fresh.iter().copied() {
            merge::relink_successor(mac, tx, &mut stmts, market, resolution, bucket, &fresh)?;
        }
    }
    Ok((inserted, stmts.into_rewrites()))
}
