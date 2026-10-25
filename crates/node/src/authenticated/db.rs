//! Bounded schema inspection and hardened, independent SQLite connections.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use rusqlite::config::DbConfig;
use rusqlite::limits::Limit;
use rusqlite::{Connection, OpenFlags};

use crate::authenticated::{Inner, MAX_STORED_VALUE, Runtime, Store, WriteFiles, geometry};

const APPLICATION_ID: i64 = 0x45414a31;
const FORMAT_VERSION: i64 = 3;
const NODES: &str = "CREATE TABLE nodes (
    namespace BLOB NOT NULL CHECK(typeof(namespace) = 'blob' AND length(namespace) = 32),
    version INTEGER NOT NULL CHECK(typeof(version) = 'integer' AND version >= 0),
    path BLOB NOT NULL CHECK(typeof(path) = 'blob' AND length(path) <= 256),
    data BLOB NOT NULL CHECK(typeof(data) = 'blob' AND length(data) <= 16384),
    PRIMARY KEY(namespace, version, path)
) WITHOUT ROWID";
const VALUES: &str = "CREATE TABLE value_rows (
    namespace BLOB NOT NULL CHECK(typeof(namespace) = 'blob' AND length(namespace) = 32),
    key BLOB NOT NULL CHECK(typeof(key) = 'blob' AND length(key) = 32),
    version INTEGER NOT NULL CHECK(typeof(version) = 'integer' AND version >= 0),
    data BLOB NOT NULL CHECK(typeof(data) = 'blob' AND length(data) <= 4194304),
    PRIMARY KEY(namespace, key, version)
) WITHOUT ROWID";
/// Lets activation delete the versions after a signed root by range, not by scanning every value.
const VALUE_VERSIONS: &str = "CREATE INDEX value_versions ON value_rows(namespace, version)";

impl Store {
    /// Exclusively creates a fresh database file with the authenticated-store schema.
    /// Error cases:
    /// - the path exists, its parent is absent, or initialization/durability fails
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = absolute(path.as_ref())?;
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        let ownership_lock = ownership_lock(&path)?;
        let mut conn = open_sql(&path, Mode::Write)?;
        let mode: String = conn.pragma_update_and_check(None, "journal_mode", "DELETE", |r| r.get(0))?;
        ensure!(mode.eq_ignore_ascii_case("delete"), "rollback journal unavailable");
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        let tx = conn.transaction()?;
        tx.execute_batch(NODES)?;
        tx.execute_batch(VALUES)?;
        tx.execute_batch(VALUE_VERSIONS)?;
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        tx.pragma_update(None, "user_version", FORMAT_VERSION)?;
        validate(&tx)?;
        tx.commit()?;
        let store = Self::from_parts(path, Some(WriteFiles { database: file, _ownership_lock: ownership_lock }), false);
        store.sync()?;
        Ok(store)
    }

    /// Opens and validates an existing cache strictly read-only, without creating a lock sidecar.
    /// A crash mid-transaction can leave a hot rollback journal; the store then reads the database
    /// file as it is and leaves the journal untouched, and every read is still checked against a
    /// signed root. `enable_writes` rolls the journal back.
    /// Error cases:
    /// - missing/corrupt format, malformed geometry, or WAL sidecars
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self> {
        let path = absolute(path.as_ref())?;
        let crash_image = geometry::check_crash_image(&path)?;
        let store = Self::from_parts(path, None, crash_image);
        let mut conn = store.read_connection()?;
        let tx = conn.transaction()?;
        validate(&tx)?;
        tx.commit()?;
        Ok(store)
    }

    /// Opens a read-only connection, reading a crash image without its journal.
    /// Error cases:
    /// - SQLite cannot open the file or apply its defensive settings
    pub(super) fn read_connection(&self) -> Result<Connection> {
        let mode = if self.inner.crash_image.load(Ordering::Acquire) { Mode::CrashImage } else { Mode::Read };
        open_sql(&self.inner.path, mode)
    }

    /// Authorizes later writes; the parent calls this only after all #24 recovery checks succeed.
    /// Under the ownership lock, it first rolls back a transaction a crash left unfinished.
    /// Error cases:
    /// - poisoned writer, lock contention, failed rollback, malformed cache or unsafe sidecar state
    pub fn enable_writes(&self) -> Result<()> {
        let _writer = self.inner.writer.lock().map_err(|_| anyhow::anyhow!("writer poisoned"))?;
        let mut files = self.inner.files.lock().map_err(|_| anyhow::anyhow!("write authorization poisoned"))?;
        if files.is_some() {
            return Ok(());
        }
        let lock = ownership_lock(&self.inner.path)?;
        if geometry::hot_journal(&self.inner.path)? {
            // SQLite rolls a hot journal back when a writable connection first reads.
            let mut conn =
                open_sql(&self.inner.path, Mode::Write).context("rolling back an interrupted transaction")?;
            let tx = conn.transaction()?;
            validate(&tx)?;
            tx.commit()?;
        }
        geometry::check(&self.inner.path)?;
        let database = OpenOptions::new().read(true).write(true).open(&self.inner.path)?;
        let mut conn = connect(&self.inner.path, false)?;
        let tx = conn.transaction()?;
        validate(&tx)?;
        tx.commit()?;
        *files = Some(WriteFiles { database, _ownership_lock: lock });
        self.inner.crash_image.store(false, Ordering::Release);
        Ok(())
    }

    /// Requires explicit write authorization without deriving root authority from disk.
    /// Error cases:
    /// - preflight has not authorized writes or the authorization lock is poisoned
    pub(super) fn writable(&self) -> Result<()> {
        ensure!(
            self.inner.files.lock().map_err(|_| anyhow::anyhow!("write authorization poisoned"))?.is_some(),
            "read-only cache; enable writes only after recovery preflight"
        );
        Ok(())
    }

    /// Builds a read-only or explicitly authorized store with no discovered disk roots.
    fn from_parts(path: PathBuf, files: Option<WriteFiles>, crash_image: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                path,
                crash_image: AtomicBool::new(crash_image),
                files: Mutex::new(files),
                writer: Mutex::new(()),
                runtime: Mutex::new(Runtime::default()),
            }),
        }
    }

    /// Completes file and parent-directory durability before acknowledgement.
    /// Error cases:
    /// - the platform/filesystem refuses either sync
    pub(super) fn sync(&self) -> Result<()> {
        let files = self.inner.files.lock().map_err(|_| anyhow::anyhow!("write authorization poisoned"))?;
        files
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("read-only store"))?
            .database
            .sync_all()
            .context("syncing authenticated database")?;
        let parent = self.inner.path.parent().ok_or_else(|| anyhow::anyhow!("store lacks parent"))?;
        File::open(parent)?.sync_all().context("syncing authenticated database directory")
    }
}

/// Locks a companion file before opening an existing database descriptor.
/// Error cases:
/// - a nonregular lock path, IO failure, or another live owner
fn ownership_lock(path: &Path) -> Result<File> {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    let path = PathBuf::from(name);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(metadata.file_type().is_file(), "ownership lock is not a regular file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // Closing an unrelated database fd can release SQLite's process-wide POSIX record locks.
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    file.try_lock().map_err(|e| anyhow::anyhow!("locking authenticated database owner: {e}"))?;
    Ok(file)
}

/// How a connection opens the database file.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Write,
    Read,
    /// Read-only and `immutable`: SQLite takes no lock and ignores any journal, so a hot journal is
    /// neither rolled back nor refused. Only for stores no process writes until `enable_writes`.
    CrashImage,
}

/// Opens one bounded-cache connection; callers validate schema inside their own transaction.
/// Error cases:
/// - SQLite cannot open the existing file or apply its defensive settings
pub(super) fn connect(path: &Path, writable: bool) -> Result<Connection> {
    open_sql(path, if writable { Mode::Write } else { Mode::Read })
}

/// Configures the native SQLite limits before any schema use; creation alone accepts an empty file.
/// Error cases:
/// - SQLite open/configuration fails, or a crash image's path is not UTF-8
fn open_sql(path: &Path, mode: Mode) -> Result<Connection> {
    let writable = mode == Mode::Write;
    let conn = match mode {
        Mode::Write => {
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)?
        }
        Mode::Read => {
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?
        }
        Mode::CrashImage => {
            let text = path.to_str().context("crash image path is not UTF-8")?;
            // `?` and `#` end a URI path; `%` starts an escape.
            let escaped = text.replace('%', "%25").replace('?', "%3f").replace('#', "%23");
            Connection::open_with_flags(
                format!("file://{escaped}?mode=ro&immutable=1"),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?
        }
    };
    // A reader waits out the writer's commit, which syncs twice; a short timeout turned a slow disk
    // into failed history pages.
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, (MAX_STORED_VALUE + 32768) as i32)?;
    conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 32768)?;
    conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, 16)?;
    conn.set_limit(Limit::SQLITE_LIMIT_EXPR_DEPTH, 64)?;
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    conn.set_limit(Limit::SQLITE_LIMIT_VDBE_OP, 10000)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW, false)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    if !writable {
        // Host TOCTOU can expose WAL after header preflight; forbid shared-memory side effects too.
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
        conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
        conn.pragma_update(None, "query_only", true)?;
    }
    conn.pragma_update(None, "cell_size_check", true)?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "mmap_size", 0)?;
    conn.pragma_update(None, "cache_size", -2048)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(conn)
}

/// Rejects extra schema objects and altered SQL with constant-size result bounds.
/// Error cases:
/// - any format identifier, schema object, or required journal setting differs
pub(super) fn validate(conn: &Connection) -> Result<()> {
    let id: i64 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let mode: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    let sync: i64 = conn.pragma_query_value(None, "synchronous", |r| r.get(0))?;
    let vacuum: i64 = conn.pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
    ensure!(id == APPLICATION_ID && version == FORMAT_VERSION, "unsupported authenticated-store format");
    ensure!(mode.eq_ignore_ascii_case("delete") && sync == 2 && vacuum == 2, "unsafe journal settings");
    let mut stmt = conn.prepare(
        "SELECT type, name, tbl_name, CASE WHEN length(sql) <= 4096 THEN sql ELSE NULL END
         FROM sqlite_schema ORDER BY type, name LIMIT 4",
    )?;
    let mut rows = stmt.query([])?;
    let expected = [
        ("index", "value_versions", "value_rows", VALUE_VERSIONS),
        ("table", "nodes", "nodes", NODES),
        ("table", "value_rows", "value_rows", VALUES),
    ];
    for (kind, name, table, sql) in expected {
        let row = rows.next()?.ok_or_else(|| anyhow::anyhow!("missing schema object"))?;
        ensure!(
            row.get_ref(0)?.as_str()? == kind
                && row.get_ref(1)?.as_str()? == name
                && row.get_ref(2)?.as_str()? == table
                && row.get_ref(3)?.as_str()? == sql,
            "unexpected authenticated-store schema"
        );
    }
    ensure!(rows.next()?.is_none(), "extra authenticated-store schema object");
    Ok(())
}

/// Resolves relative paths before handing the store to blocking workers.
/// Error cases:
/// - the working directory cannot be read or the path has no filename
fn absolute(path: &Path) -> Result<PathBuf> {
    ensure!(path.file_name().is_some(), "database path lacks filename");
    Ok(if path.is_absolute() { path.to_owned() } else { std::env::current_dir()?.join(path) })
}
