//! The marks cache on disk: one SQLite file beside the history index. `marks` holds one row per
//! `(market, resolution, bucket)`: the window's OHLC and its provenance, so a re-taken sample
//! rewrites the same row. `backfilled` records which markets a provider backfill completed, so a
//! restart does not refetch. The host can rewrite the file, so every connection runs SQLite in
//! defensive mode with triggers and views off, and every write and read refuses, inside its own
//! transaction, a schema this code did not create. Nothing here is authoritative: the log and the
//! provider are, and either rebuilds this cache.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tokio::sync::Semaphore;
use tokio::task::spawn_blocking;
use tracing::warn;

use crate::history::marks::{MarkEntry, MarkSample, MarksPage, Provenance, Resolution, Retentions};
use crate::history::store::{self, SchemaRow, read_only};

/// The SQLite file of the cache, in `history.dir`; SQLite keeps its `-wal` and `-shm` files beside it.
pub const MARKS_FILE: &str = "marks.sqlite";
/// The schema this code writes, kept in the file's `user_version`.
const SCHEMA_VERSION: i64 = 1;
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
        PRIMARY KEY (market, resolution, bucket)
    ) WITHOUT ROWID;
    CREATE TABLE backfilled (
        market INTEGER PRIMARY KEY,
        venue TEXT NOT NULL,
        symbol TEXT NOT NULL,
        at INTEGER NOT NULL
    );
";
/// A window read: the bucket and its OHLC, as stored.
const WINDOW: &str = "SELECT bucket, open, high, low, close, provenance FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket >= :from AND bucket <= :to
    ORDER BY bucket ASC LIMIT :limit";
/// A row's stored prices and provenance, for merging a sample into an existing bucket.
const EXISTING: &str = "SELECT open, high, low, close, provenance FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket = :bucket";
/// How long a connection waits on another's lock before it fails.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Pages read at once, each on its own connection.
const MAX_READS: usize = 16;
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
    /// What `SCHEMA` creates, which every write checks the file against.
    schema: Vec<SchemaRow>,
}

/// Reads pages of the cache, each on a new read-only connection.
#[derive(Clone)]
pub struct Reader {
    path: PathBuf,
    permits: Arc<Semaphore>,
    /// What `SCHEMA` creates, which every read checks the file against.
    schema: Arc<[SchemaRow]>,
}

impl Store {
    /// Opens the cache in `dir`, on `MARKS_FILE` beside the history index.
    /// Error cases:
    /// - as for `open` (see `open_checked`)
    pub fn open_in(dir: &Path, retentions: Retentions) -> anyhow::Result<Self> {
        Self::open(&dir.join(MARKS_FILE), retentions)
    }

    /// A reader of this cache for the API, on its own read-only connections; the store keeps
    /// writing for the task that owns it.
    pub fn reader(&self) -> Reader {
        Reader::new(self.path.clone())
    }

    /// Opens the cache at `path`, creating its directory, the file, and the schema if missing.
    /// A file that cannot be opened, holds another version of the schema, or holds a schema this
    /// code did not create, is discarded and rebuilt: the cache is derived, so nothing in it
    /// outranks a rebuild.
    /// Error cases:
    /// - a fresh file cannot be created in place of a discarded one
    pub fn open(path: &Path, retentions: Retentions) -> anyhow::Result<Self> {
        let found_file = path.exists();
        match Self::open_checked(path, retentions) {
            Ok(store) => Ok(store),
            Err(e) if !found_file => Err(e),
            Err(e) => {
                warn!(alert = "marks_cache_discarded", path = %path.display(), "rebuilding the marks cache: {e:#}");
                store::discard(path)?;
                Self::open_checked(path, retentions)
            }
        }
    }

    /// Opens the cache at `path`, creating its directory, the file, and the schema if missing.
    /// One transaction checks the schema and the integrity of the whole file.
    /// Error cases:
    /// - the directory or the file cannot be created or opened, or the file is not a SQLite database
    /// - the file holds another version of the schema, or a schema this code did not create
    /// - SQLite's integrity check finds the file damaged
    fn open_checked(path: &Path, retentions: Retentions) -> anyhow::Result<Self> {
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
        Ok(Self { conn, path: path.to_owned(), retentions, schema })
    }

    /// Takes `samples`, log-derived marks a command left, in sequenced time order. In one
    /// transaction, which holds the write lock from its start and first checks the schema: merges
    /// each sample into its bucket at every resolution — open kept, high and low widened, close
    /// moved, the bucket now log-derived — then drops buckets older than the retention policy
    /// allows, counted back from the newest sample.
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
                "INSERT OR REPLACE INTO marks (market, resolution, bucket, open, high, low, close, provenance)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )
            .context("preparing a marks store")?;
        for &(market, ts, mark) in samples {
            for resolution in Resolution::ALL {
                merge(&mut existing, &mut replace, market, resolution, resolution.bucket(ts), mark)?;
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
                    "INSERT OR IGNORE INTO marks (market, resolution, bucket, open, high, low, close, provenance)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .context("preparing a marks seed")?;
            for row in rows {
                let provenance = Provenance::SeededFromProvider.as_str();
                let values = params![
                    i64::from(row.market),
                    row.resolution as i64,
                    row.ts,
                    row.open.to_string(),
                    row.high.to_string(),
                    row.low.to_string(),
                    row.close.to_string(),
                    provenance,
                ];
                inserted += insert.execute(values).with_context(|| format!("seeding {}", row.market))?;
            }
        }
        prune(&tx, &self.retentions, newest)?;
        tx.commit().context("committing a marks seed")?;
        Ok(inserted)
    }

    /// The markets a provider backfill has completed for, so a restart does not refetch.
    /// Error cases:
    /// - the markets cannot be read
    pub fn backfilled(&self) -> anyhow::Result<BTreeSet<MarketId>> {
        let mut query = self.conn.prepare("SELECT market FROM backfilled").context("reading backfilled markets")?;
        let rows: Vec<i64> = query
            .query_map([], |row| row.get(0))
            .context("reading backfilled markets")?
            .collect::<Result<_, _>>()
            .context("reading backfilled markets")?;
        let markets = rows
            .iter()
            .map(|market| u32::try_from(*market).with_context(|| format!("a stored market {market} is out of range")))
            .collect::<anyhow::Result<BTreeSet<_>>>()?;
        Ok(markets)
    }

    /// Records that `market`'s backfill from `venue` for `symbol` completed at `at`.
    /// Error cases:
    /// - the marker cannot be written
    pub fn mark_backfilled(&mut self, market: MarketId, venue: &str, symbol: &str, at: Millis) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO backfilled (market, venue, symbol, at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (market) DO UPDATE SET venue = excluded.venue, symbol = excluded.symbol, at = excluded.at",
                params![market, venue, symbol, at],
            )
            .with_context(|| format!("recording the backfill of {market}"))?;
        Ok(())
    }
}

/// Merges `mark` into the `(market, resolution, bucket)` row: kept open, widened high and low,
/// moved close, and the bucket now log-derived. A row the host altered to unparseable prices
/// fails the write, which then changes nothing.
/// Error cases:
/// - the transaction fails, or a stored price does not parse
fn merge(
    existing: &mut rusqlite::Statement,
    replace: &mut rusqlite::Statement,
    market: MarketId,
    resolution: Resolution,
    bucket: Millis,
    mark: Fixed,
) -> anyhow::Result<()> {
    let row = existing
        .query_row(params![i64::from(market), resolution as i64, bucket], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?))
        })
        .optional()
        .with_context(|| format!("reading the {market} {resolution:?} bucket at {bucket}"))?;
    let (open, high, low, close) = match row {
        None => (mark, mark, mark, mark),
        Some((open, high, low, _)) => {
            let fixed =
                |text: &str| text.parse::<Fixed>().with_context(|| format!("a stored mark {text:?} does not parse"));
            let open = fixed(&open)?;
            (open, fixed(&high)?.max(mark), fixed(&low)?.min(mark), mark)
        }
    };
    replace
        .execute(params![
            i64::from(market),
            resolution as i64,
            bucket,
            open.to_string(),
            high.to_string(),
            low.to_string(),
            close.to_string(),
            Provenance::LogDerived.as_str(),
        ])
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

impl Reader {
    /// A reader of the cache at `path`, which the writer has created.
    pub fn new(path: PathBuf) -> Self {
        Self { path, permits: Arc::new(Semaphore::new(MAX_READS)), schema: expected_schema().into() }
    }

    /// One page of `market`'s marks at `resolution`, oldest first, over the inclusive window
    /// [`from`, `to`], at most `limit` entries plus the lookahead that decides `next`; `max_limit`
    /// is the response cap the page documents. It is read on a blocking thread holding one of
    /// `MAX_READS` permits, which it keeps until the read ends even if the request is dropped.
    /// Error cases:
    /// - the cache cannot be opened or read, holds another schema, or holds a row that does not parse
    pub async fn page(
        &self,
        market: MarketId,
        resolution: Resolution,
        from: Millis,
        to: Millis,
        limit: usize,
        max_limit: usize,
    ) -> anyhow::Result<MarksPage> {
        let permit = self.permits.clone().acquire_owned().await.expect("the read semaphore is never closed");
        let (path, schema) = (self.path.clone(), self.schema.clone());
        let read = spawn_blocking(move || {
            let _permit = permit;
            read_page(&path, &schema, market, resolution, from, to, limit, max_limit)
        });
        read.await.context("marks reader task")?
    }
}

/// Reads one page from the cache at `path`, once it holds exactly `schema`.
/// Error cases:
/// - the cache cannot be opened or read, holds another schema, or holds a row that does not parse
fn read_page(
    path: &Path,
    schema: &[SchemaRow],
    market: MarketId,
    resolution: Resolution,
    from: Millis,
    to: Millis,
    limit: usize,
    max_limit: usize,
) -> anyhow::Result<MarksPage> {
    let mut conn = read_only(path)?;
    // The schema check and the page read see one snapshot of the file.
    let conn = conn.transaction().context("starting a marks read")?;
    store::check_schema(&conn, schema, path)?;
    // One more than the page, to learn whether a newer page follows.
    let fetched = i64::try_from(limit).context("page limit")?.saturating_add(1);
    let mut stmt = conn.prepare(WINDOW).context("preparing a marks page")?;
    let rows = stmt
        .query_map(params![i64::from(market), resolution as i64, from, to, fetched], |row| {
            Ok((
                row.get::<_, Millis>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .context("reading a marks page")?;
    let mut entries: Vec<MarkEntry> = rows
        .map(|row| {
            let (ts, open, high, low, close, provenance) = row.context("reading a marks row")?;
            let parse = |text: String| text.parse::<Fixed>().with_context(|| format!("a stored mark {text:?}"));
            let provenance =
                Provenance::parse(&provenance).with_context(|| format!("a stored provenance {provenance:?}"))?;
            Ok(MarkEntry {
                ts,
                open: parse(open)?,
                high: parse(high)?,
                low: parse(low)?,
                close: parse(close)?,
                provenance,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    let next = if entries.len() > limit {
        entries.truncate(limit);
        entries.last().map(|entry| entry.ts + 1)
    } else {
        None
    };
    Ok(MarksPage { market, resolution: resolution.as_str(), entries, next, max_limit })
}

/// What `SCHEMA` creates, as `sqlite_schema` lists it.
fn expected_schema() -> Vec<SchemaRow> {
    let conn = Connection::open_in_memory().expect("SQLite opens an in-memory database");
    conn.execute_batch(SCHEMA).expect("the marks schema builds");
    store::schema_of(&conn).expect("an in-memory schema reads")
}

#[cfg(test)]
mod tests;
