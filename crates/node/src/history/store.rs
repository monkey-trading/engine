//! The history index on disk: one SQLite file. `events` holds each account event under its place in
//! the log, `(seq, idx)`, so taking a command twice rewrites the same rows; `progress` holds, in the
//! same transaction as the events, the last command taken and the retention the rows were pruned
//! under. The host can rewrite the file, so every connection runs SQLite in defensive mode with
//! triggers and views off, and every write and read refuses, inside its own transaction, a schema
//! this code did not create.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use engine_core::types::Millis;
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags, OptionalExtension, ToSql, TransactionBehavior, params};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::spawn_blocking;
use tracing::{debug, info};

use crate::history::{HistoryEvent, Page, PageQuery, account_of};
use crate::log::{CommandEntry, Offset};
use crate::sequencer::{StreamBody, StreamMsg};

/// The schema this code writes, kept in the file's `user_version`.
const SCHEMA_VERSION: i64 = 2;
const SCHEMA: &str = "
    CREATE TABLE events (
        seq INTEGER NOT NULL,
        idx INTEGER NOT NULL,
        account BLOB NOT NULL,
        kind TEXT NOT NULL,
        ts INTEGER NOT NULL,
        event TEXT NOT NULL,
        PRIMARY KEY (seq, idx)
    ) WITHOUT ROWID;
    CREATE INDEX events_by_account ON events (account, seq, idx);
    CREATE INDEX events_by_account_kind ON events (account, kind, seq, idx);
    CREATE INDEX events_by_ts ON events (ts);
    CREATE TABLE progress (
        id INTEGER PRIMARY KEY CHECK (id = 0),
        command TEXT NOT NULL,
        retention_ms INTEGER NOT NULL
    );
";
/// A page, newest first. `INDEXED BY` makes SQLite refuse the query rather than plan a scan, so a
/// page reads at most `:limit` entries of the index.
const PAGE: &str = "SELECT seq, idx, ts, event FROM events INDEXED BY events_by_account
    WHERE account = :account AND (seq, idx) < (:before_seq, :before_idx) AND (seq, idx) > (:after_seq, :after_idx)
    ORDER BY seq DESC, idx DESC LIMIT :limit";
const PAGE_OF_KIND: &str = "SELECT seq, idx, ts, event FROM events INDEXED BY events_by_account_kind
    WHERE account = :account AND kind = :kind
        AND (seq, idx) < (:before_seq, :before_idx) AND (seq, idx) > (:after_seq, :after_idx)
    ORDER BY seq DESC, idx DESC LIMIT :limit";
/// Most commands written in one transaction.
const WRITE_BATCH: usize = 1024;
/// How long a connection waits on another's lock before it fails.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Pages read at once, each on its own connection.
const MAX_READS: usize = 16;

/// One row of `sqlite_schema`: its type, name, table, and SQL.
type SchemaRow = (String, String, String, Option<String>);

/// Takes commands into the index: the one connection that writes to it.
pub(super) struct Writer {
    conn: Connection,
    path: PathBuf,
    retention_ms: Millis,
    /// What `SCHEMA` creates, which every write checks the file against.
    schema: Vec<SchemaRow>,
}

/// What the index has taken: the last command, and the retention its rows were pruned under.
pub(super) struct Progress {
    pub(super) command: CommandEntry,
    pub(super) retention_ms: Millis,
}

/// Reads pages of the index, each on a new read-only connection.
#[derive(Clone)]
pub struct Reader {
    path: PathBuf,
    permits: Arc<Semaphore>,
    /// Set once the index has taken every command the log held when the node started.
    ready: watch::Receiver<bool>,
    /// What `SCHEMA` creates, which every read checks the file against.
    schema: Arc<[SchemaRow]>,
}

impl Writer {
    /// Opens the index at `path`, creating its directory, the file, and the schema if missing, and
    /// returns it with what it has taken. One transaction checks the schema and the integrity of
    /// the whole file, and reads that. Events leave the index `retention_ms` before the newest
    /// command it holds.
    /// Error cases:
    /// - the directory or the file cannot be created or opened, or the file is not a SQLite database
    /// - the file holds another version of the schema, or a schema this code did not create
    /// - SQLite's integrity check finds the file damaged
    /// - the command the index holds does not parse
    pub(super) fn open(path: &Path, retention_ms: Millis) -> anyhow::Result<(Self, Option<Progress>)> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        harden(&conn)?;
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
                tx.execute_batch(SCHEMA).context("creating the history schema")?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION).context("setting the schema version")?;
            }
            SCHEMA_VERSION => {}
            other => bail!("{} holds history schema {other}, not {SCHEMA_VERSION}", path.display()),
        }
        check_schema(&tx, &schema, path)?;
        // A torn page, or an index that no longer matches its table, would fail a later write or
        // serve another row; one bad entry is enough to refuse the file.
        let integrity: String =
            tx.query_row("PRAGMA integrity_check(1)", [], |row| row.get(0)).context("checking the history index")?;
        ensure!(integrity == "ok", "{} is damaged: {integrity}", path.display());
        let row: Option<(String, Millis)> = tx
            .query_row("SELECT command, retention_ms FROM progress WHERE id = 0", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()
            .context("reading history progress")?;
        let progress = row
            .map(|(command, retention_ms)| {
                let command = serde_json::from_str(&command).context("history progress: the last command taken")?;
                anyhow::Ok(Progress { command, retention_ms })
            })
            .transpose()?;
        tx.commit().context("committing the history schema")?;
        Ok((Self { conn, path: path.to_owned(), retention_ms, schema }, progress))
    }

    /// Takes commands from `rx` until every sender is gone, writing whatever is queued in one
    /// transaction at a time. Sets `ready` once a command at or past `ready_at` is written.
    /// Error cases:
    /// - a write fails (see `write`)
    pub(super) fn run(
        mut self,
        mut rx: mpsc::Receiver<Arc<StreamMsg>>,
        ready: watch::Sender<bool>,
        ready_at: Option<u64>,
    ) -> anyhow::Result<()> {
        let mut batch = Vec::new();
        while let Some(msg) = rx.blocking_recv() {
            batch.push(msg);
            while batch.len() < WRITE_BATCH
                && let Ok(msg) = rx.try_recv()
            {
                batch.push(msg);
            }
            let written = self.write(&batch)?;
            if let (Some(at), Some(seq)) = (ready_at, written)
                && seq >= at
                && !ready.send_replace(true)
            {
                info!(seq, "history index caught up with the log");
            }
            batch.clear();
        }
        Ok(())
    }

    /// In one transaction, which holds the write lock from its start and first checks the schema:
    /// stores the account events of the commands in `batch`, records the last command as taken, and
    /// drops events older than the retention allows. Returns the seq of that command, if `batch`
    /// holds one.
    /// Error cases:
    /// - the file holds a schema this code did not create
    /// - the transaction fails
    /// - a seq is beyond SQLite's integer range, or an event or command cannot be encoded
    pub(super) fn write(&mut self, batch: &[Arc<StreamMsg>]) -> anyhow::Result<Option<u64>> {
        let commands: Vec<&CommandEntry> = batch
            .iter()
            .filter_map(|msg| match &msg.body {
                StreamBody::Command(command) => Some(command),
                _ => None,
            })
            .collect();
        let Some(last) = commands.last() else {
            return Ok(None);
        };
        let tx =
            self.conn.transaction_with_behavior(TransactionBehavior::Immediate).context("starting a history write")?;
        check_schema(&tx, &self.schema, &self.path)?;
        {
            let mut insert = tx
                .prepare("INSERT OR REPLACE INTO events (seq, idx, account, kind, ts, event) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
                .context("preparing the history insert")?;
            for command in &commands {
                let (seq, ts) = (i64::try_from(command.sequenced.seq).context("seq")?, command.sequenced.ts);
                for (idx, event) in command.events.iter().flatten().enumerate() {
                    let Some(account) = account_of(event) else {
                        continue;
                    };
                    let json = serde_json::to_value(event).context("encoding an event")?;
                    let kind = json["type"].as_str().expect("events serialize with a type tag").to_owned();
                    let row = params![seq, idx as i64, &account.0[..], kind, ts, json.to_string()];
                    insert.execute(row).with_context(|| format!("storing event {seq}:{idx}"))?;
                }
            }
        }
        let taken = serde_json::to_string(last).context("encoding a command")?;
        tx.execute(
            "INSERT INTO progress (id, command, retention_ms) VALUES (0, ?1, ?2)
             ON CONFLICT (id) DO UPDATE SET command = excluded.command, retention_ms = excluded.retention_ms",
            params![taken, self.retention_ms],
        )
        .context("storing history progress")?;
        let horizon = last.sequenced.ts.saturating_sub(self.retention_ms);
        tx.execute("DELETE FROM events WHERE ts < ?1", [horizon]).context("dropping events past retention")?;
        tx.commit().context("committing a history write")?;
        debug!(seq = last.sequenced.seq, commands = commands.len(), "history index took commands");
        Ok(Some(last.sequenced.seq))
    }
}

impl Reader {
    /// A reader of the index at `path`, which the writer creates, that serves pages once `ready`
    /// is set.
    pub fn new(path: PathBuf, ready: watch::Receiver<bool>) -> Self {
        let schema = expected_schema().into();
        Self { path, permits: Arc::new(Semaphore::new(MAX_READS)), ready, schema }
    }

    /// One page of `query`, or `None` while the index is still taking the commands the log held
    /// when the node started. It is read on a blocking thread holding one of `MAX_READS` permits,
    /// which it keeps until the read ends even if the request is dropped.
    /// Error cases:
    /// - the index cannot be opened or read, or holds a row that does not parse
    pub async fn page(&self, query: PageQuery) -> anyhow::Result<Option<Page>> {
        if !*self.ready.borrow() {
            return Ok(None);
        }
        let permit = self.permits.clone().acquire_owned().await.expect("the read semaphore is never closed");
        let (path, schema) = (self.path.clone(), self.schema.clone());
        let read = spawn_blocking(move || {
            let _permit = permit;
            read_page(&path, &schema, &query)
        });
        read.await.context("history reader task")?.map(Some)
    }
}

/// Reads one page of `query` from the index at `path`, once it holds exactly `schema`.
/// Error cases:
/// - the index cannot be opened or read, holds another schema, or holds a row that does not parse
fn read_page(path: &Path, schema: &[SchemaRow], query: &PageQuery) -> anyhow::Result<Page> {
    let mut conn = open_reader(path)?;
    // The schema check and the page read see one snapshot of the file.
    let conn = conn.transaction().context("starting a history read")?;
    check_schema(&conn, schema, path)?;
    let account = &query.account.0[..];
    let (before_seq, before_idx) = bound(query.before, i64::MAX);
    let (after_seq, after_idx) = bound(query.after, -1);
    // One more than the page, to learn whether an older page follows.
    let limit = i64::try_from(query.limit).context("page limit")?.saturating_add(1);
    let mut args: Vec<(&str, &dyn ToSql)> = vec![
        (":account", &account),
        (":before_seq", &before_seq),
        (":before_idx", &before_idx),
        (":after_seq", &after_seq),
        (":after_idx", &after_idx),
        (":limit", &limit),
    ];
    let sql = match &query.kind {
        Some(kind) => {
            args.push((":kind", kind));
            PAGE_OF_KIND
        }
        None => PAGE,
    };
    let mut stmt = conn.prepare(sql).context("preparing a history page")?;
    let rows = stmt
        .query_map(args.as_slice(), |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get::<_, String>(3)?)))
        .context("reading a history page")?;
    let mut events = rows
        .map(|row| {
            let (seq, idx, ts, event): (i64, i64, Millis, String) = row.context("reading a history row")?;
            let row = || format!("history row {seq}:{idx}");
            let cursor =
                Offset { seq: u64::try_from(seq).with_context(row)?, index: u32::try_from(idx).with_context(row)? };
            let event = serde_json::from_str(&event).with_context(|| format!("history event {cursor}"))?;
            Ok(HistoryEvent { cursor, ts, event })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let next = if events.len() > query.limit {
        events.truncate(query.limit);
        events.last().map(|event| event.cursor)
    } else {
        None
    };
    Ok(Page { events, next })
}

/// Removes the index at `path`, then the write-ahead log and shared-memory files SQLite keeps beside
/// it: once the database file is gone, SQLite discards a write-ahead log left without one.
/// Error cases:
/// - a file exists but cannot be removed
pub(super) fn discard(path: &Path) -> anyhow::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        if let Err(e) = fs::remove_file(&file)
            && e.kind() != ErrorKind::NotFound
        {
            return Err(e).with_context(|| format!("removing {}", Path::new(&file).display()));
        }
    }
    Ok(())
}

/// A read-only connection to the index at `path`, hardened.
/// Error cases:
/// - the file cannot be opened, or a protection cannot be set
fn open_reader(path: &Path) -> anyhow::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags).with_context(|| format!("opening {}", path.display()))?;
    harden(&conn)?;
    conn.busy_timeout(BUSY_TIMEOUT).context("setting the busy timeout")?;
    Ok(conn)
}

/// Sets what every connection to the host's file runs with: defensive mode, which refuses SQL that
/// could corrupt the file; no triggers or views, which this schema has none of, so none the host
/// adds can run; schema SQL kept from calling functions with side effects; and cell sizes checked as
/// pages are read.
/// Error cases:
/// - SQLite refuses a setting
pub(super) fn harden(conn: &Connection) -> anyhow::Result<()> {
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true).context("enabling defensive mode")?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false).context("disabling triggers")?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW, false).context("disabling views")?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false).context("distrusting the schema")?;
    conn.pragma_update(None, "cell_size_check", true).context("enabling cell size checks")
}

/// What `SCHEMA` creates, as `sqlite_schema` lists it.
pub(super) fn expected_schema() -> Vec<SchemaRow> {
    let conn = Connection::open_in_memory().expect("SQLite opens an in-memory database");
    conn.execute_batch(SCHEMA).expect("the history schema builds");
    schema_of(&conn).expect("an in-memory schema reads")
}

/// The schema of `conn`'s database, as `sqlite_schema` lists it.
/// Error cases:
/// - the schema cannot be read
fn schema_of(conn: &Connection) -> anyhow::Result<Vec<SchemaRow>> {
    let mut stmt = conn
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name")
        .context("reading the schema")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)));
    rows.context("reading the schema")?.collect::<Result<_, _>>().context("reading the schema")
}

/// Checks that the database at `path` holds exactly `expected`, so no table, index, trigger, or view
/// the host added runs.
/// Error cases:
/// - the schema cannot be read, or differs from `expected`
pub(super) fn check_schema(conn: &Connection, expected: &[SchemaRow], path: &Path) -> anyhow::Result<()> {
    ensure!(schema_of(conn)? == expected, "{} holds a schema this code did not create", path.display());
    Ok(())
}

/// A cursor as the `(seq, idx)` SQLite compares, or `(absent, absent)` without one. Every stored
/// seq fits an `i64`, so a larger one bounds like the largest.
fn bound(cursor: Option<Offset>, absent: i64) -> (i64, i64) {
    cursor.map_or((absent, absent), |c| (i64::try_from(c.seq).unwrap_or(i64::MAX), i64::from(c.index)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use engine_core::Fixed;
    use engine_core::command::{Command, Sequenced};
    use engine_core::event::Event;
    use engine_core::types::AccountId;

    use crate::log::LogEntry;
    use crate::test_support::{assert_a_dropped_read_keeps_its_permit, scratch_dir};

    /// A schema the host altered is refused when the writer opens the file and when a reader opens
    /// it later, so no trigger, view, or index it added runs.
    #[tokio::test]
    async fn a_schema_this_code_did_not_create_is_refused_by_the_writer_and_every_read() {
        let alterations = [
            "CREATE TRIGGER wipe AFTER INSERT ON events BEGIN DELETE FROM events; END",
            "CREATE VIEW recent AS SELECT * FROM events",
            "CREATE INDEX extra ON events (kind)",
            "DROP INDEX events_by_ts",
        ];
        for (n, alteration) in alterations.into_iter().enumerate() {
            let path = scratch_dir(&format!("history-schema-{n}")).join("history.sqlite");
            let writer = Writer::open(&path, 1).unwrap().0;
            Connection::open(&path).unwrap().execute_batch(alteration).unwrap();
            let query = PageQuery { account: AccountId([1; 20]), kind: None, before: None, after: None, limit: 1 };
            let read = Reader::new(path.clone(), watch::channel(true).1).page(query).await;
            assert!(format!("{:#}", read.unwrap_err()).contains("did not create"), "{alteration}");
            drop(writer);
            let err = Writer::open(&path, 1).err().expect("refused");
            assert!(format!("{err:#}").contains("did not create"), "{alteration}: {err:#}");
        }
    }

    /// A schema the host changes while the node runs stops the writer's next write, which then
    /// changes nothing; the trigger it added never runs.
    #[test]
    fn a_schema_changed_while_the_writer_runs_stops_its_next_write() {
        let path = scratch_dir("history-schema-live").join("history.sqlite");
        let mut writer = Writer::open(&path, i64::MAX).unwrap().0;
        let deposit = |seq| {
            let events = vec![Event::Deposited { account: AccountId([1; 20]), amount: Fixed::from_int(1) }];
            let sequenced = Sequenced { seq, ts: 0, cmd: Command::Tick };
            Arc::new(StreamMsg::new(
                Offset::start_of(seq),
                LogEntry::Command(CommandEntry { seal: None, sequenced, events: Some(events) }),
            ))
        };
        writer.write(&[deposit(1)]).unwrap();
        let wipe = "CREATE TRIGGER wipe AFTER INSERT ON events BEGIN DELETE FROM events; END";
        Connection::open(&path).unwrap().execute_batch(wipe).unwrap();
        let err = writer.write(&[deposit(2)]).expect_err("refused");
        assert!(format!("{err:#}").contains("did not create"), "{err:#}");
        let taken: String = writer.conn.query_row("SELECT command FROM progress", [], |row| row.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<CommandEntry>(&taken).unwrap().sequenced.seq, 1);
        let rows: i64 = writer.conn.query_row("SELECT count(*) FROM events", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 1, "the first write's event is still there");
    }

    #[test]
    fn every_connection_to_the_index_runs_defensive_with_an_untrusted_schema() {
        let path = scratch_dir("history-defensive").join("history.sqlite");
        let writer = Writer::open(&path, 1).unwrap().0;
        let reader = open_reader(&path).unwrap();
        for (side, conn) in [("writer", &writer.conn), ("reader", &reader)] {
            assert!(conn.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE).unwrap(), "{side}");
            assert!(!conn.db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER).unwrap(), "{side}");
            assert!(!conn.db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW).unwrap(), "{side}");
            assert!(!conn.db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA).unwrap(), "{side}");
            let checked: bool = conn.pragma_query_value(None, "cell_size_check", |row| row.get(0)).unwrap();
            assert!(checked, "{side}");
        }
        // Defensive mode refuses to rewrite the schema even with writable_schema on.
        let rewrite = "PRAGMA writable_schema = ON; UPDATE sqlite_schema SET sql = sql WHERE name = 'events'";
        assert!(writer.conn.execute_batch(rewrite).is_err());
    }

    #[tokio::test]
    async fn a_dropped_page_request_keeps_its_permit_until_its_read_ends() {
        // Without a write-ahead log, a held write lock stalls readers too.
        let dir = scratch_dir("history-read-permit");
        fs::create_dir_all(&dir).unwrap();
        let lock = Connection::open(dir.join("history.sqlite")).unwrap();
        lock.execute_batch(SCHEMA).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let reader = Reader::new(dir.join("history.sqlite"), watch::channel(true).1);
        let query = PageQuery { account: AccountId([1; 20]), kind: None, before: None, after: None, limit: 1 };
        let release = || lock.execute_batch("ROLLBACK").unwrap();
        assert_a_dropped_read_keeps_its_permit(&reader.permits, reader.page(query), release).await;
    }

    #[test]
    fn a_page_reads_its_account_index_in_order_without_sorting() {
        let path = scratch_dir("history-plan").join("history.sqlite");
        let writer = Writer::open(&path, 1).unwrap().0;
        for (sql, index) in [(PAGE, "events_by_account"), (PAGE_OF_KIND, "events_by_account_kind")] {
            let mut explain = writer.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let mut rows = explain.raw_query();
            let mut plan: Vec<String> = Vec::new();
            while let Some(row) = rows.next().unwrap() {
                plan.push(row.get(3).unwrap());
            }
            // One search of the index, bounded on both sides by the cursors, in index order.
            assert_eq!(plan.len(), 1, "{plan:?}");
            assert!(plan[0].starts_with(&format!("SEARCH events USING INDEX {index} (")), "{plan:?}");
            assert!(plan[0].contains("(seq,idx)>(?,?) AND (seq,idx)<(?,?)"), "{plan:?}");
        }
    }
}
