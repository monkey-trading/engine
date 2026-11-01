//! Opening the cache: creating or checking the schema, discarding a file that fails the check,
//! and establishing the committed anchors a reopened file carries.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail, ensure};
use hmac::digest::KeyInit as _;

use rusqlite::{Connection, TransactionBehavior};
use tracing::warn;

use crate::history::marks::Retentions;
use crate::history::marks::store::{
    BUSY_TIMEOUT, Reader, SCHEMA, SCHEMA_VERSION, Store, expected_schema, reset, store,
};

impl Store {
    /// Opens the cache in `dir`, on `MARKS_FILE` beside the history index, authenticating rows
    /// with `mac_key`. Test construction site; startup seeds on the hidden file and activates.
    /// Error cases:
    /// - as for `open` (see `open_checked`)
    #[cfg(test)]
    pub fn open_in(dir: &Path, retentions: Retentions, mac_key: [u8; 32]) -> anyhow::Result<Self> {
        Self::open(&dir.join(super::MARKS_FILE), retentions, mac_key)
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
        let store = Self { conn, path: path.to_owned(), retentions, mac, mac_key, schema, anchors: Arc::default() };
        // An existing file (a reopened cache in tests; production opens a fresh seed) gets its
        // anchors established here; re-derivation at recovery validates them before any read.
        store.establish_anchors()?;
        Ok(store)
    }

    /// A reader of this cache for the API, on its own read-only connections; the store keeps
    /// writing for the task that owns it. The reader shares the committed chain anchors, so every
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

    /// Validates every pair's chain on disk: every row authenticates and every link names the
    /// row actually below it. Startup runs this after importing a prior cache, so a host-altered
    /// file is rebuilt from nothing — the log and a configured backfill re-derive it — instead of
    /// serving refusals or stopping the node on a later write.
    /// Error cases:
    /// - a stored row fails authentication, or a chain link does not resolve
    pub fn validate_chains(&mut self) -> anyhow::Result<()> {
        let tx =
            self.conn.transaction_with_behavior(TransactionBehavior::Immediate).context("starting a marks check")?;
        store::check_schema(&tx, &self.schema, &self.path)?;
        reset::validate_chains(&self.mac, &tx)
    }
}
