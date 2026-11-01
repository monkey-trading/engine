//! The marks cache on disk: one SQLite file beside the history index. `marks` holds one row per
//! `(market, resolution, bucket)`: the window's OHLC, its provenance, and its chained-index link
//! naming the bucket below it, so a re-taken sample rewrites the same row and a host that deletes
//! or rolls rows back breaks the chain the next read walks. `backfilled` records which markets a
//! provider backfill completed, so a restart does not refetch. The host can rewrite the file, so
//! every connection runs SQLite in defensive mode with triggers and views off, and every write
//! and read refuses, inside its own transaction, a schema this code did not create. Nothing here
//! is authoritative: the log and the provider are, and either rebuilds this cache.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};
use rusqlite::{Connection, TransactionBehavior, params};
use tracing::{debug, warn};

mod auth;
mod chain;
mod import;
mod merge;
mod reader;
mod reset;
mod setup;

pub(crate) use auth::{ChainAnchor, ChainHead, Coverage};
use auth::{Row, tag};
pub use reader::Reader;

use crate::history::marks::{MarkSample, Resolution, Retentions};
use crate::history::store::{self, SchemaRow, read_only};

/// The SQLite file of the cache, in `history.dir`; SQLite keeps its `-wal` and `-shm` files beside it.
pub const MARKS_FILE: &str = "marks.sqlite";
/// The schema this code writes, kept in the file's `user_version`. Version 2 authenticated every
/// row: `tag` is an HMAC of the row's fields under the enclave key, so a row the host rewrote —
/// while the node was stopped or running — fails verification instead of being served or merged.
/// Version 3 chains the rows of each (market, resolution) into one index: `prev_bucket` names the
/// bucket below the row and the tag binds it, so a host that deletes, reorders, or rolls buckets
/// back breaks the chain the next read refuses on, and per-pair chain anchors — the floor and
/// head committed in memory, atomically with each write — reject a restored old snapshot before
/// any of it is served. Rows strictly between an unchanged floor and head are bound by the links
/// walked per page; what a bounded read never spans, the next recovery's re-derivation rewrites.
pub(crate) const SCHEMA_VERSION: i64 = 3;
pub(crate) const SCHEMA: &str = "
    CREATE TABLE marks (
        market INTEGER NOT NULL,
        resolution INTEGER NOT NULL,
        bucket INTEGER NOT NULL,
        open TEXT NOT NULL,
        high TEXT NOT NULL,
        low TEXT NOT NULL,
        close TEXT NOT NULL,
        provenance TEXT NOT NULL,
        prev_bucket INTEGER,
        tag BLOB NOT NULL,
        PRIMARY KEY (market, resolution, bucket)
    ) WITHOUT ROWID;
    CREATE INDEX marks_by_window ON marks (resolution, bucket);
    CREATE TABLE backfilled (
        market INTEGER PRIMARY KEY,
        venue TEXT NOT NULL,
        symbol TEXT NOT NULL,
        at INTEGER NOT NULL,
        depth_1m INTEGER NOT NULL,
        depth_1h INTEGER NOT NULL,
        depth_1d INTEGER NOT NULL,
        retention_1s INTEGER NOT NULL,
        retention_1m INTEGER NOT NULL,
        retention_1h INTEGER NOT NULL,
        retention_1d INTEGER NOT NULL
    );
";
/// A window read: the bucket, its OHLC, its chain link, and its authentication tag, as stored.
const WINDOW: &str = "SELECT bucket, open, high, low, close, provenance, prev_bucket, tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket >= :from AND bucket <= :to
    ORDER BY bucket ASC LIMIT :limit";
/// How long a connection waits on another's lock before it fails.
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Pages read at once, each on its own connection.
const MAX_READS: usize = 16;

/// One page's market, resolution, inclusive window, and size.
struct PageQuery {
    market: MarketId,
    resolution: Resolution,
    from: Millis,
    to: Millis,
    limit: usize,
    max_limit: usize,
}

/// One stored row as the chain maintenance reasons over it: its bucket, its stored content and
/// chain link, and its authentication tag.
pub(crate) struct SnapRow {
    pub bucket: Millis,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub provenance: String,
    pub prev: Option<Millis>,
    pub tag: [u8; 16],
}

/// The committed chain anchors of every (market, resolution) pair, established at recovery
/// re-derivation and advanced by every write, atomically with its commit. The store and its
/// readers share one map: the writer merges in the pairs a transaction touched, a reader checks
/// one pair's floor and head before serving a page.
pub(crate) type ChainAnchors = BTreeMap<(MarketId, Resolution), ChainAnchor>;

/// Removes the cache file at `path` and its write-ahead side files, as `history::store` does
/// for the index. Startup uses it to clear a seed file a killed run left behind.
/// Error cases:
/// - a file exists but cannot be removed
pub(crate) fn discard_file(path: &Path) -> anyhow::Result<()> {
    store::discard(path)
}

/// A provider-seeded bucket.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Seed {
    pub market: MarketId,
    pub resolution: Resolution,
    pub ts: Millis,
    pub open: Fixed,
    pub high: Fixed,
    pub low: Fixed,
    pub close: Fixed,
}

/// The one connection that writes the cache, owned by the marks task; recovery seeds through it
/// before the task starts.
pub struct Store {
    conn: Connection,
    path: PathBuf,
    retentions: Retentions,
    /// Authenticates rows on write and verifies them on read; derived from the enclave key.
    mac: hmac::Hmac<sha2::Sha256>,
    /// The key `mac` derives from, kept to rebuild the cache after a refused reset.
    mac_key: [u8; 32],
    /// What `SCHEMA` creates, which every write checks the file against.
    schema: Vec<SchemaRow>,
    /// The committed chain anchors this store advances with every write and every read checks.
    anchors: Arc<Mutex<ChainAnchors>>,
}

impl Store {
    /// The committed anchors as they stand: what a transaction validates the touched pairs
    /// against before it moves them.
    fn prior_anchors(&self) -> ChainAnchors {
        self.anchors.lock().expect("the marks chain anchors are never poisoned").clone()
    }

    /// Establishes the committed anchors of every pair on disk. Called only at open, before any
    /// reader exists: a fresh or crash-leftover seed file is the best available start, and the
    /// recovery re-derivation that follows validates it before serving. Every later write merges
    /// in only the pairs it touched, derived from its own transaction (see `publish`).
    /// Error cases:
    /// - the pairs or their floors and heads cannot be read
    fn establish_anchors(&self) -> anyhow::Result<()> {
        let mut pairs = self
            .conn
            .prepare("SELECT market, resolution FROM marks GROUP BY market, resolution")
            .context("reading the marks pairs")?;
        let pairs = pairs
            .query_map([], |row| Ok((row.get::<_, MarketId>(0)?, row.get::<_, i64>(1)?)))
            .context("reading the marks pairs")?
            .collect::<Result<Vec<_>, _>>()
            .context("reading the marks pairs")?;
        let mut anchors = BTreeMap::new();
        for (market, resolution) in pairs {
            let resolution =
                Resolution::from_tag(u8::try_from(resolution).context("a stored resolution is out of range")?)
                    .context("a stored resolution is out of range")?;
            if let Some(anchor) = chain::pair_anchor(&self.conn, market, resolution)? {
                anchors.insert((market, resolution), anchor);
            }
        }
        *self.anchors.lock().expect("the marks chain anchors are never poisoned") = anchors;
        Ok(())
    }

    /// Takes `samples`, log-derived marks a command left, in sequenced time order. In one
    /// transaction, which holds the write lock from its start and first checks the schema: merges
    /// each sample into its bucket at every resolution — open kept, high and low widened, close
    /// moved, the bucket now log-derived, a provider-seeded bucket replaced by the first log mark
    /// it receives — then drops buckets older than the retention policy allows, counted back from
    /// the newest sample.
    /// Error cases:
    /// - the file holds a schema this code did not create
    /// - the transaction fails, a stored price does not parse, or a chain link no longer resolves
    pub fn observe(&mut self, samples: &[MarkSample]) -> anyhow::Result<()> {
        let Some(&newest) = samples.iter().map(|(_, ts, _)| ts).max() else { return Ok(()) };
        let prior = self.prior_anchors();
        let tx =
            self.conn.transaction_with_behavior(TransactionBehavior::Immediate).context("starting a marks write")?;
        store::check_schema(&tx, &self.schema, &self.path)?;
        let mut touched: BTreeSet<(MarketId, Resolution)> = samples
            .iter()
            .map(|&(market, _, _)| market)
            .flat_map(|market| Resolution::ALL.map(move |resolution| (market, resolution)))
            .collect();
        // Validate the pre-merge state of every pair the write can touch: the pairs the samples
        // merge, and any pair whose committed floor a prune would cut. A merge writes only the
        // samples' pairs, so every pair with rows to prune after it is among these.
        touched.extend(chain::expiring_pairs(&prior, &self.retentions, newest));
        chain::check_anchors(&tx, &prior, &touched)?;
        for &(market, ts, mark) in samples {
            for resolution in Resolution::ALL {
                merge::merge(&self.mac, &tx, market, resolution, resolution.bucket(ts), mark)?;
            }
        }
        let plan = chain::plan_prune(&tx, &self.retentions, newest, &touched)?;
        chain::apply_prune(&self.mac, &tx, &plan)?;
        let updates = chain::anchor_updates(&tx, &touched)?;
        // The commit and the anchor publication are one critical section: a reader that sees an
        // older state than the anchor raced this flush and re-synchronizes instead of refusing.
        let mut anchors = self.anchors.lock().expect("the marks chain anchors are never poisoned");
        tx.commit().context("committing a marks write")?;
        apply_updates(&mut anchors, updates);
        Ok(())
    }

    /// Seeds `rows`, provider candles turned into buckets, keeping every row already there: seeded
    /// history never overwrites log-derived marks or an earlier seed. Returns how many rows were
    /// added. Drops seeded buckets older than the retention policy allows, counted back from
    /// `newest`.
    /// Error cases:
    /// - as for `observe`, or a stored row fails authentication
    pub fn seed(&mut self, rows: &[Seed], newest: Millis) -> anyhow::Result<usize> {
        let prior = self.prior_anchors();
        let tx =
            self.conn.transaction_with_behavior(TransactionBehavior::Immediate).context("starting a marks seed")?;
        store::check_schema(&tx, &self.schema, &self.path)?;
        let mut touched = chain::expiring_pairs(&prior, &self.retentions, newest);
        touched.extend(rows.iter().map(|row| (row.market, row.resolution)));
        chain::check_anchors(&tx, &prior, &touched)?;
        let inserted = chain::seed(&self.mac, &tx, rows)?;
        let plan = chain::plan_prune(&tx, &self.retentions, newest, &touched)?;
        chain::apply_prune(&self.mac, &tx, &plan)?;
        let updates = chain::anchor_updates(&tx, &touched)?;
        let mut anchors = self.anchors.lock().expect("the marks chain anchors are never poisoned");
        tx.commit().context("committing a marks seed")?;
        apply_updates(&mut anchors, updates);
        Ok(inserted)
    }

    /// Deletes every log-derived bucket, so a recovery re-derivation rebuilds exactly what the
    /// verified log proves and nothing the host wrote into those rows while the node was stopped;
    /// provider-seeded buckets stay until a log mark replaces them, and the chains over them
    /// re-link only over what this delete removed. A refused reset rebuilds the derived cache
    /// from nothing.
    /// Error cases:
    /// - the cache file cannot be discarded or reopened
    pub fn reset_log_derived(&mut self) -> anyhow::Result<()> {
        if let Err(e) = self.try_reset_log_derived() {
            // The cache is derived, never authoritative: a chain the reset refuses is host-altered
            // state the verified log cannot vouch for, and the log plus a configured backfill
            // re-derive it — discard the file and reset empty, rather than refuse the boot on
            // rebuildable state.
            warn!(alert = "marks_cache_rebuilt", "rebuilding the marks cache after a refused reset: {e:#}");
            let path = self.path.clone();
            store::discard(&path)?;
            // Readers cloned before the rebuild share this store's anchor map: keep them on one
            // map, reset to what the fresh, empty file commits.
            let shared = self.anchors.clone();
            *self = Self::open(&path, self.retentions, self.mac_key)
                .with_context(|| format!("rebuilding the marks cache after a refused reset: {e:#}"))?;
            let fresh = self.prior_anchors();
            self.anchors = shared;
            *self.anchors.lock().expect("the marks chain anchors are never poisoned") = fresh;
        }
        Ok(())
    }

    /// Deletes every log-derived bucket, validating the chains first (see `reset.rs`).
    /// Error cases:
    /// - the transaction fails, a stored row fails authentication, a chain link names a bucket
    ///   nothing deleted, or a pair no longer matches its committed anchor
    fn try_reset_log_derived(&mut self) -> anyhow::Result<()> {
        let prior = self.prior_anchors();
        let tx =
            self.conn.transaction_with_behavior(TransactionBehavior::Immediate).context("starting a marks reset")?;
        store::check_schema(&tx, &self.schema, &self.path)?;
        let (removed, updates) = reset::reset_log_derived(&self.mac, &tx, &prior)?;
        let mut anchors = self.anchors.lock().expect("the marks chain anchors are never poisoned");
        tx.commit().context("committing a marks reset")?;
        apply_updates(&mut anchors, updates);
        debug!(removed, "log-derived marks reset for re-derivation");
        Ok(())
    }

    /// The provider backfills completed so far, by market, with the coverage each recorded. A
    /// backfill re-runs when the config asks for different depths or its retentions have grown
    /// past what the recorded backfill could leave behind.
    /// Error cases:
    /// - the markers cannot be read
    pub fn backfilled(&self) -> anyhow::Result<BTreeMap<MarketId, Coverage>> {
        let mut query = self
            .conn
            .prepare(
                "SELECT market, depth_1m, depth_1h, depth_1d, retention_1s, retention_1m, retention_1h, retention_1d
                 FROM backfilled",
            )
            .context("reading backfilled markets")?;
        let rows: Vec<[Millis; 8]> = query
            .query_map([], |row| {
                Ok([
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ])
            })
            .context("reading backfilled markets")?
            .collect::<Result<_, _>>()
            .context("reading backfilled markets")?;
        rows.iter()
            .map(|row| {
                let market =
                    u32::try_from(row[0]).with_context(|| format!("a stored market {} is out of range", row[0]))?;
                let d1 = (row[7] >= 0).then_some(row[7]);
                Ok((market, Coverage { depths: (row[1], row[2], row[3]), retentions: (row[4], row[5], row[6], d1) }))
            })
            .collect()
    }

    /// Records that `market`'s backfill from `venue` for `symbol` completed at `at`, covering
    /// `coverage` (the depths the config asked for and the retentions that pruned the seed).
    /// Error cases:
    /// - the marker cannot be written
    pub fn mark_backfilled(
        &mut self,
        market: MarketId,
        venue: &str,
        symbol: &str,
        at: Millis,
        coverage: Coverage,
    ) -> anyhow::Result<()> {
        let d1 = coverage.retentions.3.unwrap_or(-1);
        self.conn
            .execute(
                "INSERT INTO backfilled
                     (market, venue, symbol, at, depth_1m, depth_1h, depth_1d, retention_1s, retention_1m, retention_1h, retention_1d)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT (market) DO UPDATE SET venue = excluded.venue, symbol = excluded.symbol,
                     at = excluded.at, depth_1m = excluded.depth_1m, depth_1h = excluded.depth_1h, depth_1d = excluded.depth_1d,
                     retention_1s = excluded.retention_1s, retention_1m = excluded.retention_1m,
                     retention_1h = excluded.retention_1h, retention_1d = excluded.retention_1d",
                params![
                    i64::from(market),
                    venue,
                    symbol,
                    at,
                    coverage.depths.0,
                    coverage.depths.1,
                    coverage.depths.2,
                    coverage.retentions.0,
                    coverage.retentions.1,
                    coverage.retentions.2,
                    d1,
                ],
            )
            .with_context(|| format!("recording the backfill of {market}"))?;
        Ok(())
    }
}

/// Merges a transaction's anchor updates into the committed map: `Some` replaces the pair's
/// anchor, `None` removes a pair whose every bucket the transaction deleted.
fn apply_updates(anchors: &mut ChainAnchors, updates: chain::ChainUpdates) {
    for (pair, anchor) in updates {
        match anchor {
            Some(anchor) => {
                anchors.insert(pair, anchor);
            }
            None => {
                anchors.remove(&pair);
            }
        }
    }
}

/// What `SCHEMA` creates, as `sqlite_schema` lists it.
pub(crate) fn expected_schema() -> Vec<SchemaRow> {
    let conn = Connection::open_in_memory().expect("SQLite opens an in-memory database");
    conn.execute_batch(SCHEMA).expect("the marks schema builds");
    store::schema_of(&conn).expect("an in-memory schema reads")
}

#[cfg(test)]
mod tests;
