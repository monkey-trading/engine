//! Read-only history planning before authenticated log and custody preflight permits writes.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, ensure};
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::config::HistorySection;
use crate::history::store::{check_schema, expected_schema, harden};
use crate::history::{History, HistoryCheck, INDEX_FILE};
use crate::log::{CommandEntry, LogEntry};

pub(crate) struct HistoryPlan {
    dir: PathBuf,
    retention_ms: engine_core::types::Millis,
    progress: anyhow::Result<Option<CommandEntry>>,
    found: Option<bool>,
    authenticated: Option<crate::history::startup::Plan>,
}

impl HistoryPlan {
    /// Plans history recovery without creating or updating any SQLite files, including WAL/SHM.
    /// Error cases:
    /// - the configured retention is invalid
    pub fn open(cfg: &HistorySection) -> anyhow::Result<Self> {
        let retention_ms = cfg.retention_ms()?;
        let path = cfg.dir.join(INDEX_FILE);
        let progress = (|| {
            if !path.exists() {
                return Ok(None);
            }
            let metadata = std::fs::metadata(&path).context("reading history database metadata")?;
            ensure!(metadata.is_file(), "history database is not a regular file");
            // SQLite may remove an existing WAL while opening a zero-page main database,
            // even on a read-only connection. Such an image must wait for authorized repair.
            ensure!(metadata.len() > 0, "history database is empty");
            validate_geometry(&path, metadata.len())?;
            // A read-only no-lock VFS plus EXCLUSIVE mode gives SQLite a private heap WAL
            // index. It reads committed WAL pages without creating/updating the original SHM,
            // unlike an ordinary read-only open. Actual writable progress is rechecked below.
            // SQLite documents this mode at https://sqlite.org/wal.html#noshm.
            let mut uri = reqwest::Url::from_file_path(std::path::absolute(&path)?)
                .map_err(|_| anyhow::anyhow!("history path is not absolute"))?;
            let has_wal =
                path.with_file_name(format!("{INDEX_FILE}-wal")).try_exists().context("checking history WAL")?;
            if has_wal {
                uri.query_pairs_mut().append_pair("vfs", "unix-none");
            } else {
                // SQLite may create an empty WAL even for a read-only normal WAL-mode open.
                // With no existing WAL, immutable mode reads the complete database itself.
                uri.query_pairs_mut().append_pair("immutable", "1");
            }
            let conn = Connection::open_with_flags(
                uri.as_str(),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            if has_wal {
                conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
                    .context("disabling preflight close checkpoints")?;
                conn.pragma_update(None, "locking_mode", "exclusive").context("using a private read-only WAL index")?;
            }
            harden(&conn)?;
            check_schema(&conn, &expected_schema(), &path)?;
            let progress: Option<(String, engine_core::types::Millis)> = conn
                .query_row("SELECT command, retention_ms FROM progress WHERE id = 0", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()?;
            progress
                .map(|(command, retention)| {
                    ensure!(retention == retention_ms, "history retention differs from config");
                    serde_json::from_str(&command).context("reading history progress")
                })
                .transpose()
        })();
        Ok(Self { dir: cfg.dir.clone(), retention_ms, progress, found: None, authenticated: None })
    }

    /// Keeps paired recovery entirely separate from the legacy eager SQL integrity scan.
    /// Error cases:
    /// - configured retention is invalid
    pub(crate) fn authenticated(plan: crate::history::startup::Plan) -> anyhow::Result<Self> {
        Ok(Self {
            dir: plan.history.dir.clone(),
            retention_ms: plan.history.retention_ms()?,
            progress: Ok(None),
            found: None,
            authenticated: Some(plan),
        })
    }

    /// Finishes only after Node's independent preflight has authorized all persisted mutations.
    /// Error cases:
    /// - authorized legacy recovery or authenticated generation/catalog activation fails
    pub(crate) fn finish_with_log(
        mut self,
        log: crate::sequencer::ReplayedLog,
        key: std::sync::Arc<crate::tee::EnclaveKey>,
    ) -> anyhow::Result<(crate::sequencer::ReplayedLog, History)> {
        match self.authenticated.take() {
            Some(plan) => plan.activate(log, key),
            None => Ok((log, self.finish()?)),
        }
    }

    /// Matches progress against the same authenticated stream that recovers the engine.
    pub fn observe(&mut self, entry: &LogEntry) {
        if let (LogEntry::Command(command), Ok(Some(last))) = (entry, &self.progress)
            && command.sequenced.seq == last.sequenced.seq
        {
            self.found = Some(command == last);
        }
    }

    /// Opens or repairs history only after the caller has authorized persisted mutations.
    /// Error cases:
    /// - SQLite open or authorized repair fails
    pub fn finish(self) -> anyhow::Result<History> {
        ensure!(self.authenticated.is_none(), "paired history requires authorized log-bound activation");
        let mut check = HistoryCheck::open(&HistorySection {
            dir: self.dir,
            retention: std::time::Duration::from_millis(u64::try_from(self.retention_ms)?),
            marks: None,
        })?;
        match self.progress {
            Err(reason) => check.opened = Err(reason),
            Ok(expected) => {
                check.opened = check.opened.and_then(|(writer, actual)| {
                    ensure!(actual.as_ref() == expected.as_ref(), "history progress changed during preflight");
                    Ok((writer, actual))
                });
                check.found = self.found;
                if self.found == Some(true)
                    && let Some(last) = expected
                {
                    check.observe(&LogEntry::Command(last));
                }
            }
        }
        check.finish()
    }
}

/// SQLite's Unix VFS maps a one-byte file to zero bytes; its pager also narrows
/// the page count to u32. Either zero-page interpretation can unlink recovery
/// evidence even during a read-only open. Validate the header without SQLite.
fn validate_geometry(path: &std::path::Path, len: u64) -> anyhow::Result<()> {
    let mut header = [0; 100];
    std::fs::File::open(path)?.read_exact(&mut header).context("reading history database header")?;
    ensure!(&header[..16] == b"SQLite format 3\0", "invalid history database header");
    let encoded = u16::from_be_bytes([header[16], header[17]]);
    let page_size = if encoded == 1 { 65_536_u64 } else { u64::from(encoded) };
    ensure!((512..=65_536).contains(&page_size) && page_size.is_power_of_two(), "invalid history page size");
    ensure!(len >= page_size, "history database is smaller than one page");
    ensure!(len.div_ceil(page_size) < u64::from(u32::MAX), "history database page count exceeds SQLite limit");
    Ok(())
}

#[cfg(test)]
mod tests;
