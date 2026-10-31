//! The marks cache on disk: one SQLite file beside the history index. `marks` holds one row per
//! `(market, resolution, bucket)`: the window's OHLC and its provenance, so a re-taken sample
//! rewrites the same row. `backfilled` records which markets a provider backfill completed, so a
//! restart does not refetch. The host can rewrite the file, so every connection runs SQLite in
//! defensive mode with triggers and views off, and every write and read refuses, inside its own
//! transaction, a schema this code did not create. Nothing here is authoritative: the log and the
//! provider are, and either rebuilds this cache.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};
use hmac::digest::KeyInit as _;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tracing::{debug, warn};

mod auth;
mod import;
mod reader;

pub(crate) use auth::Coverage;
use auth::{Row, tag};
pub use reader::Reader;

use crate::history::marks::{MarkSample, Provenance, Resolution, Retentions};
use crate::history::store::{self, SchemaRow, read_only};

/// The SQLite file of the cache, in `history.dir`; SQLite keeps its `-wal` and `-shm` files beside it.
pub const MARKS_FILE: &str = "marks.sqlite";
/// The schema this code writes, kept in the file's `user_version`. Version 2 authenticates every
/// row: `tag` is an HMAC of the row's fields under the enclave key, so a row the host rewrote —
/// while the node was stopped or running — fails verification instead of being served or merged.
const SCHEMA_VERSION: i64 = 2;
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
/// A window read: the bucket, its OHLC, and its authentication tag, as stored.
const WINDOW: &str = "SELECT bucket, open, high, low, close, provenance, tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket >= :from AND bucket <= :to
    ORDER BY bucket ASC LIMIT :limit";
/// A row's stored prices, provenance, and tag, for merging a sample into an existing bucket.
const EXISTING: &str = "SELECT open, high, low, close, provenance, tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket = :bucket";
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
        Ok(Self { conn, path: path.to_owned(), retentions, mac, schema })
    }

    /// A reader of this cache for the API, on its own read-only connections; the store keeps
    /// writing for the task that owns it.
    pub fn reader(&self) -> Reader {
        Reader::new(self.path.clone(), self.mac.clone())
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

    /// Takes `samples`, log-derived marks a command left, in sequenced time order. In one
    /// transaction, which holds the write lock from its start and first checks the schema: merges
    /// each sample into its bucket at every resolution — open kept, high and low widened, close
    /// moved, the bucket now log-derived, a provider-seeded bucket replaced by the first log mark
    /// it receives — then drops buckets older than the retention policy allows, counted back from
    /// the newest sample.
    /// Error cases:
    /// - the file holds a schema this code did not create
    /// - the transaction fails, or a stored price does not parse
    pub fn observe(&mut self, samples: &[MarkSample]) -> anyhow::Result<()> {
        let Some(&newest) = samples.iter().map(|(_, ts, _)| ts).max() else { return Ok(()) };
        let tx =
            self.conn.transaction_with_behavior(TransactionBehavior::Immediate).context("starting a marks write")?;
        store::check_schema(&tx, &self.schema, &self.path)?;
        let mut existing = tx.prepare(EXISTING).context("preparing a marks merge")?;
        let mut replace = tx
            .prepare(
                "INSERT OR REPLACE INTO marks (market, resolution, bucket, open, high, low, close, provenance, tag)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .context("preparing a marks store")?;
        for &(market, ts, mark) in samples {
            for resolution in Resolution::ALL {
                merge(&self.mac, &mut existing, &mut replace, market, resolution, resolution.bucket(ts), mark)?;
            }
        }
        drop((existing, replace));
        prune(&tx, &self.retentions, newest)?;
        tx.commit().context("committing a marks write")?;
        Ok(())
    }

    /// Seeds `rows`, provider candles turned into buckets, keeping every row already there: seeded
    /// history never overwrites log-derived marks or an earlier seed. Returns how many rows were
    /// added. Drops seeded buckets older than the retention policy allows, counted back from
    /// `newest`.
    /// Error cases:
    /// - as for `observe`
    pub fn seed(&mut self, rows: &[Seed], newest: Millis) -> anyhow::Result<usize> {
        let tx =
            self.conn.transaction_with_behavior(TransactionBehavior::Immediate).context("starting a marks seed")?;
        store::check_schema(&tx, &self.schema, &self.path)?;
        let mut inserted = 0;
        {
            let mut insert = tx
                .prepare(
                    "INSERT OR IGNORE INTO marks (market, resolution, bucket, open, high, low, close, provenance, tag)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .context("preparing a marks seed")?;
            for row in rows {
                let provenance = Provenance::SeededFromProvider.as_str();
                let (open, high, low, close) =
                    (row.open.to_string(), row.high.to_string(), row.low.to_string(), row.close.to_string());
                let tag = tag(
                    &self.mac,
                    row.market,
                    row.resolution,
                    row.ts,
                    &Row { open: &open, high: &high, low: &low, close: &close, provenance },
                );
                let values = params![
                    i64::from(row.market),
                    row.resolution as i64,
                    row.ts,
                    open,
                    high,
                    low,
                    close,
                    provenance,
                    tag,
                ];
                inserted += insert.execute(values).with_context(|| format!("seeding {}", row.market))?;
            }
        }
        prune(&tx, &self.retentions, newest)?;
        tx.commit().context("committing a marks seed")?;
        Ok(inserted)
    }

    /// Deletes every log-derived bucket, so a recovery re-derivation rebuilds exactly what the
    /// verified log proves and nothing the host wrote into those rows while the node was stopped;
    /// provider-seeded buckets stay until a log mark replaces them. One transaction, checked
    /// against the schema.
    /// Error cases:
    /// - the delete fails
    pub fn reset_log_derived(&mut self) -> anyhow::Result<()> {
        let tx =
            self.conn.transaction_with_behavior(TransactionBehavior::Immediate).context("starting a marks reset")?;
        store::check_schema(&tx, &self.schema, &self.path)?;
        let removed = tx
            .execute("DELETE FROM marks WHERE provenance = ?1", [Provenance::LogDerived.as_str()])
            .context("resetting log-derived marks")?;
        tx.commit().context("committing a marks reset")?;
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

/// Merges `mark` into the `(market, resolution, bucket)` row: kept open, widened high and low,
/// moved close, and the bucket now log-derived. A row the host altered — to other prices, another
/// provenance, or an unparseable value — fails the write, which then changes nothing.
/// Error cases:
/// - the transaction fails, or a stored price does not parse
fn merge(
    mac: &hmac::Hmac<sha2::Sha256>,
    existing: &mut rusqlite::Statement,
    replace: &mut rusqlite::Statement,
    market: MarketId,
    resolution: Resolution,
    bucket: Millis,
    mark: Fixed,
) -> anyhow::Result<()> {
    let row = existing
        .query_row(params![i64::from(market), resolution as i64, bucket], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, [u8; 16]>(5)?,
            ))
        })
        .optional()
        .with_context(|| format!("reading the {market} {resolution:?} bucket at {bucket}"))?;
    let (open, high, low, close) = match row {
        None => (mark.to_string(), mark.to_string(), mark.to_string(), mark.to_string()),
        Some((open, high, low, close, provenance, stored)) => {
            // A row whose tag does not match its stored fields was rewritten; refuse to build on
            // it, so host edits never reach a merge or a signed answer.
            let expected = tag(
                mac,
                market,
                resolution,
                bucket,
                &Row { open: &open, high: &high, low: &low, close: &close, provenance: &provenance },
            );
            ensure!(stored == expected, "the {market} {resolution:?} bucket at {bucket} fails authentication");
            if provenance != Provenance::LogDerived.as_str() {
                // A provider-seeded bucket the log now speaks for is rebuilt from the log mark
                // alone, so a log-derived bucket is exactly what the public log proves.
                (mark.to_string(), mark.to_string(), mark.to_string(), mark.to_string())
            } else {
                let fixed = |text: &str| {
                    text.parse::<Fixed>().with_context(|| format!("a stored mark {text:?} does not parse"))
                };
                (
                    fixed(&open)?.to_string(),
                    fixed(&high)?.max(mark).to_string(),
                    fixed(&low)?.min(mark).to_string(),
                    mark.to_string(),
                )
            }
        }
    };
    let provenance = Provenance::LogDerived.as_str();
    let tag =
        tag(mac, market, resolution, bucket, &Row { open: &open, high: &high, low: &low, close: &close, provenance });
    replace
        .execute(params![i64::from(market), resolution as i64, bucket, open, high, low, close, provenance, tag,])
        .with_context(|| format!("storing the {market} {resolution:?} bucket at {bucket}"))?;
    Ok(())
}

/// Drops buckets older than their resolution's retention, counted back from `newest`.
/// Error cases:
/// - the prune fails
fn prune(tx: &rusqlite::Transaction, retentions: &Retentions, newest: Millis) -> anyhow::Result<()> {
    for resolution in Resolution::ALL {
        if let Some(retention) = retentions.of_resolution(resolution) {
            let horizon = newest.saturating_sub(retention);
            tx.execute("DELETE FROM marks WHERE resolution = ?1 AND bucket < ?2", params![resolution as i64, horizon])
                .with_context(|| format!("pruning {resolution:?} marks"))?;
        }
    }
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
