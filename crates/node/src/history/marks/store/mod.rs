//! The marks cache on disk: one SQLite file beside the history index. `marks` holds one row per
//! `(market, resolution, bucket)`: the window's OHLC, its provenance, and its chained-index link
//! naming the bucket below it, so a re-taken sample rewrites the same row and a host that deletes
//! or rolls rows back breaks the chain the next read walks. `backfilled` records which markets a
//! provider backfill completed, so a restart does not refetch. The host can rewrite the file, so
//! every connection runs SQLite in defensive mode with triggers and views off, and every write
//! and read refuses, inside its own transaction, a schema this code did not create. Nothing here
//! is authoritative: the log and the provider are, and either rebuilds this cache.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};
use hmac::digest::KeyInit as _;
use rusqlite::{Connection, TransactionBehavior, params};
use tracing::{debug, warn};

mod auth;
mod chain;
mod import;
mod reader;

pub(crate) use auth::{ChainHead, Coverage};
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
/// back breaks the chain the next read refuses on, and per-pair chain heads committed in memory
/// at recovery re-derivation reject a restored old snapshot before any of it is served.
const SCHEMA_VERSION: i64 = 3;
const SCHEMA: &str = "
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
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
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

/// The committed chain heads of every (market, resolution) pair, anchored at recovery
/// re-derivation and advanced by every write. The store and its readers share one map: the
/// writer replaces it after each commit, a reader checks one pair's head before serving a page.
pub(crate) type ChainAnchors = BTreeMap<(MarketId, Resolution), ChainHead>;

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
    /// What `SCHEMA` creates, which every write checks the file against.
    schema: Vec<SchemaRow>,
    /// The committed chain heads this store advances with every write and every read checks.
    anchors: Arc<Mutex<ChainAnchors>>,
}

impl Store {
    /// Opens the cache in `dir`, on `MARKS_FILE` beside the history index, authenticating rows
    /// with `mac_key`. Test construction site; startup seeds on the hidden file and activates.
    /// Error cases:
    /// - as for `open` (see `open_checked`)
    #[cfg(test)]
    pub fn open_in(dir: &Path, retentions: Retentions, mac_key: [u8; 32]) -> anyhow::Result<Self> {
        Self::open(&dir.join(MARKS_FILE), retentions, mac_key)
    }

    /// Opens the cache at `path`, creating its directory, the file, and the schema if missing.
    /// A file that cannot be opened, holds another version of the schema, or holds a schema this
    /// code did not create, is discarded and rebuilt: the cache is derived, so nothing in it
    /// outranks a rebuild.
    /// Error cases:
    /// - a fresh file cannot be created in place of a discarded one
    pub fn open(path: &Path, retentions: Retentions, mac_key: [u8; 32]) -> anyhow::Result<Self> {
        let found_file = path.exists();
        match Self::open_checked(path, retentions, mac_key) {
            Ok(store) => Ok(store),
            Err(e) if !found_file => Err(e),
            Err(e) => {
                warn!(alert = "marks_cache_discarded", path = %path.display(), "rebuilding the marks cache: {e:#}");
                store::discard(path)?;
                Self::open_checked(path, retentions, mac_key)
            }
        }
    }

    /// Opens the cache at `path`, creating its directory, the file, and the schema if missing.
    /// One transaction checks the schema and the integrity of the whole file.
    /// Error cases:
    /// - the directory or the file cannot be created or opened, or the file is not a SQLite database
    /// - the file holds another version of the schema, or a schema this code did not create
    /// - SQLite's integrity check finds the file damaged
    fn open_checked(path: &Path, retentions: Retentions, mac_key: [u8; 32]) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        store::harden(&conn)?;
        conn.busy_timeout(BUSY_TIMEOUT).context("setting the busy timeout")?;
        let mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "wal", |row| row.get(0))
            .with_context(|| format!("opening {}", path.display()))?;
        ensure!(mode == "wal", "{} cannot use a write-ahead log, only {mode}", path.display());
        conn.pragma_update(None, "synchronous", "normal").context("setting synchronous")?;
        let schema = expected_schema();
        let tx = conn.transaction().context("starting the schema check")?;
        match tx.pragma_query_value(None, "user_version", |row| row.get(0)).context("reading the schema version")? {
            0 => {
                tx.execute_batch(SCHEMA).context("creating the marks schema")?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION).context("setting the schema version")?;
            }
            SCHEMA_VERSION => {}
            other => bail!("{} holds marks schema {other}, not {SCHEMA_VERSION}", path.display()),
        }
        store::check_schema(&tx, &schema, path)?;
        // A torn page, or an index that no longer matches its table, would fail a later write or
        // serve another row; one bad entry is enough to refuse the file.
        let integrity: String =
            tx.query_row("PRAGMA integrity_check(1)", [], |row| row.get(0)).context("checking the marks cache")?;
        ensure!(integrity == "ok", "{} is damaged: {integrity}", path.display());
        tx.commit().context("committing the marks schema")?;
        let mac = hmac::Hmac::new_from_slice(&mac_key).context("building the marks authenticator")?;
        let store = Self { conn, path: path.to_owned(), retentions, mac, schema, anchors: Arc::default() };
        // A leftover seed file from a refused activation opens populated: its rows carry valid
        // tags, and re-derivation at recovery rewrites the log-derived ones before any read.
        store.refresh_anchors()?;
        Ok(store)
    }

    /// A reader of this cache for the API, on its own read-only connections; the store keeps
    /// writing for the task that owns it. The reader shares the committed chain heads, so every
    /// page it serves is checked against the index this store advances.
    pub fn reader(&self) -> Reader {
        Reader::new(self.path.clone(), self.mac.clone(), self.anchors.clone())
    }

    /// This cache's retention policy, recorded with backfill completion.
    pub fn retentions(&self) -> Retentions {
        self.retentions
    }

    /// The cache's file, for activation's rename.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Points this store at its file's new path after activation renames it into place; the open
    /// connection is unaffected, and readers built after this follow the new path.
    pub fn relocate(&mut self, path: PathBuf) {
        self.path = path;
    }

    /// Re-commits every pair's chain head from what is on disk, replacing the anchor map. Called
    /// at open and after every write commits: the anchors are exactly the heads of the rows this
    /// store wrote, so a file the host rolled back fails a reader's head check before it serves.
    /// Error cases:
    /// - the heads cannot be read
    fn refresh_anchors(&self) -> anyhow::Result<()> {
        let mut heads = self
            .conn
            .prepare(
                "SELECT m.market, m.resolution, m.bucket, m.tag FROM marks m
                 JOIN (SELECT market, resolution, MAX(bucket) AS head FROM marks GROUP BY market, resolution) h
                 ON m.market = h.market AND m.resolution = h.resolution AND m.bucket = h.head",
            )
            .context("reading the marks chain heads")?;
        let heads = heads
            .query_map([], |row| {
                Ok((
                    row.get::<_, MarketId>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Millis>(2)?,
                    row.get::<_, [u8; 16]>(3)?,
                ))
            })
            .context("reading the marks chain heads")?
            .collect::<Result<Vec<_>, _>>()
            .context("reading the marks chain heads")?;
        let mut anchors = BTreeMap::new();
        for (market, resolution, bucket, tag) in heads {
            let resolution =
                Resolution::from_tag(u8::try_from(resolution).context("a stored resolution is out of range")?)
                    .context("a stored resolution is out of range")?;
            anchors.insert((market, resolution), ChainHead { bucket, tag });
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
        let tx =
            self.conn.transaction_with_behavior(TransactionBehavior::Immediate).context("starting a marks write")?;
        store::check_schema(&tx, &self.schema, &self.path)?;
        for &(market, ts, mark) in samples {
            for resolution in Resolution::ALL {
                chain::merge(&self.mac, &tx, market, resolution, resolution.bucket(ts), mark)?;
            }
        }
        chain::prune(&self.mac, &tx, &self.retentions, newest)?;
        tx.commit().context("committing a marks write")?;
        self.refresh_anchors()?;
        Ok(())
    }

    /// Seeds `rows`, provider candles turned into buckets, keeping every row already there: seeded
    /// history never overwrites log-derived marks or an earlier seed. Returns how many rows were
    /// added. Drops seeded buckets older than the retention policy allows, counted back from
    /// `newest`.
    /// Error cases:
    /// - as for `observe`, or a stored row fails authentication
    pub fn seed(&mut self, rows: &[Seed], newest: Millis) -> anyhow::Result<usize> {
        let tx =
            self.conn.transaction_with_behavior(TransactionBehavior::Immediate).context("starting a marks seed")?;
        store::check_schema(&tx, &self.schema, &self.path)?;
        let inserted = chain::seed(&self.mac, &tx, rows)?;
        chain::prune(&self.mac, &tx, &self.retentions, newest)?;
        tx.commit().context("committing a marks seed")?;
        self.refresh_anchors()?;
        Ok(inserted)
    }

    /// Deletes every log-derived bucket, so a recovery re-derivation rebuilds exactly what the
    /// verified log proves and nothing the host wrote into those rows while the node was stopped;
    /// provider-seeded buckets stay until a log mark replaces them, and the chains over them
    /// re-link only over what this delete removed. One transaction, checked against the schema.
    /// Error cases:
    /// - the delete fails, or a chain link names a bucket this delete did not remove
    pub fn reset_log_derived(&mut self) -> anyhow::Result<()> {
        let tx =
            self.conn.transaction_with_behavior(TransactionBehavior::Immediate).context("starting a marks reset")?;
        store::check_schema(&tx, &self.schema, &self.path)?;
        let removed = chain::reset_log_derived(&self.mac, &tx)?;
        tx.commit().context("committing a marks reset")?;
        self.refresh_anchors()?;
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

/// What `SCHEMA` creates, as `sqlite_schema` lists it.
fn expected_schema() -> Vec<SchemaRow> {
    let conn = Connection::open_in_memory().expect("SQLite opens an in-memory database");
    conn.execute_batch(SCHEMA).expect("the marks schema builds");
    store::schema_of(&conn).expect("an in-memory schema reads")
}

#[cfg(test)]
mod tests;
