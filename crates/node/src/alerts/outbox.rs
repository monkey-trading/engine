//! Durable pages, serialized by one blocking owner. FULL SQLite commits precede network sends.
//! Schema and row checks reject damaged input; they cannot authenticate host storage or detect
//! deletion/rollback. TODO: keep this file on an integrity-protected, rollback-resistant volume.
//! Age uses the host clock across restarts; TODO: seal a trusted time watermark with the outbox.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, ensure};
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinSet, spawn_blocking};
use tracing::error;

use crate::alerts::episodes::MAX_EPISODES;
use crate::alerts::{Page, Severity, Status};
use crate::clock::now_ms;
use crate::config::OutboxSection;
use crate::metrics::{Metrics, SeverityLabel};

const PAGES_SCHEMA: &str = "CREATE TABLE pages (id INTEGER PRIMARY KEY AUTOINCREMENT, route TEXT NOT NULL, body TEXT NOT NULL);CREATE INDEX pages_by_route ON pages(route,id);";
/// Episodes whose firing page was queued and whose resolve has not been, kept across restarts so
/// the resolve still pages. Written with the page in one transaction.
const EPISODES_SCHEMA: &str =
    "CREATE TABLE episodes (id INTEGER PRIMARY KEY AUTOINCREMENT, key TEXT NOT NULL UNIQUE, severity TEXT NOT NULL);";
/// Longest episode key kept. A longer one (fields are unbounded) still pages, but its resolve does
/// not survive a restart; this bounds the table's disk use.
const MAX_EPISODE_KEY: usize = 256;
/// Bounds one serialized row, including its page and durable retention times.
const MAX_PAGE_BYTES: usize = 65_536;
/// One router and at most two delivery loops each have one request in flight.
const REQUEST_QUEUE: usize = 3;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pending {
    pub id: i64,
    pub page: Page,
    created_ms: i64,
    expires_ms: i64,
}

type Reply<T> = oneshot::Sender<anyhow::Result<T>>;

enum Request {
    Add(Page, Reply<bool>),
    Next(Severity, Reply<Option<Pending>>),
    Remove(i64, Reply<()>),
    Open(Reply<Vec<(String, Severity)>>),
}

/// Cloneable requests to the outbox's sole database owner.
#[derive(Clone)]
pub(super) struct Outbox(mpsc::Sender<Request>);

struct Store {
    conn: Connection,
    cfg: OutboxSection,
    metrics: Arc<Metrics>,
}

impl Pending {
    /// Remaining age budget; rollback during this run cannot extend the in-flight retry timer.
    pub fn remaining(&self) -> Duration {
        Duration::from_millis(self.expires_ms.saturating_sub(now_ms()).max(0) as u64)
    }
}

impl Outbox {
    /// Opens and validates the outbox before starting its supervised blocking owner.
    /// Error cases:
    /// - configuration, disk, schema, integrity, retained rows, or route validation fails
    pub async fn start(
        cfg: OutboxSection,
        routes: [bool; 2],
        metrics: Arc<Metrics>,
        tasks: &mut JoinSet<()>,
    ) -> anyhow::Result<Self> {
        let store = spawn_blocking(move || Store::open(cfg, routes, metrics)).await??;
        let (tx, mut rx) = mpsc::channel(REQUEST_QUEUE);
        let owner = spawn_blocking(move || -> anyhow::Result<()> {
            let mut store = store;
            while let Some(request) = rx.blocking_recv() {
                let result = match request {
                    Request::Add(page, reply) => respond(reply, store.add(page)),
                    Request::Next(route, reply) => respond(reply, store.next(route)),
                    Request::Remove(id, reply) => respond(reply, store.remove(id)),
                    Request::Open(reply) => respond(reply, store.open_episodes()),
                };
                result?;
            }
            Ok(())
        });
        tasks.spawn(async move {
            match owner.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => error!("alert outbox stopped: {e:#}"),
                Err(e) => error!("alert outbox owner panicked: {e}"),
            }
        });
        Ok(Self(tx))
    }

    /// Commits the page, or reports a bounded-capacity refusal without changing pending pages.
    /// Error cases:
    /// - the database owner stops or a durable write fails
    pub async fn add(&self, page: Page) -> anyhow::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.0.send(Request::Add(page, tx)).await.context("alert outbox stopped")?;
        rx.await.context("alert outbox stopped")?
    }

    /// Reads the oldest pending page of one route.
    /// Error cases:
    /// - the owner stops, or a database or row check fails
    pub async fn next(&self, route: Severity) -> anyhow::Result<Option<Pending>> {
        let (tx, rx) = oneshot::channel();
        self.0.send(Request::Next(route, tx)).await.context("alert outbox stopped")?;
        rx.await.context("alert outbox stopped")?
    }

    /// The episodes opened and not yet resolved, oldest first, including those whose firing page
    /// was already delivered.
    /// Error cases:
    /// - the owner stops, or a database or row check fails
    pub async fn open_episodes(&self) -> anyhow::Result<Vec<(String, Severity)>> {
        let (tx, rx) = oneshot::channel();
        self.0.send(Request::Open(tx)).await.context("alert outbox stopped")?;
        rx.await.context("alert outbox stopped")?
    }

    /// Durably removes a page after delivery or permanent failure.
    /// Error cases:
    /// - the owner stops or the delete cannot be committed
    pub async fn remove(&self, id: i64) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.0.send(Request::Remove(id, tx)).await.context("alert outbox stopped")?;
        rx.await.context("alert outbox stopped")?
    }
}

impl Store {
    /// Opens a private database, refusing unsupported schemas and corrupt or excessive contents.
    /// Error cases:
    /// - filesystem, SQLite, schema, route, retention, or payload validation fails
    fn open(cfg: OutboxSection, routes: [bool; 2], metrics: Arc<Metrics>) -> anyhow::Result<Self> {
        cfg.validate()?;
        std::fs::create_dir_all(&cfg.dir)?;
        let path: PathBuf = cfg.dir.join("outbox.sqlite");
        let conn = Connection::open(&path).context("opening alert outbox")?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW, false)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // Four pages of row/index slack per message also bound SQLite's retained freelist.
        let page_size: u64 = conn.pragma_query_value(None, "page_size", |r| r.get::<_, i64>(0))? as u64;
        let limit = u128::from(cfg.max_bytes).div_ceil(u128::from(page_size)) + u128::from(cfg.max_pages) * 8 + 16;
        let limit = u32::try_from(limit).context("outbox disk bound exceeds SQLite page range")?;
        let actual: u32 = conn.pragma_update_and_check(None, "max_page_count", limit, |r| r.get(0))?;
        ensure!(actual <= limit, "alert outbox file exceeds configured disk bound");
        conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
        conn.execute_batch("BEGIN EXCLUSIVE; COMMIT;")?;
        let schema: Vec<String> = conn
            .prepare("SELECT sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY name")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let schema_of = |sql: &str| -> anyhow::Result<Vec<String>> {
            let expected = Connection::open_in_memory()?;
            expected.execute_batch(sql)?;
            Ok(expected
                .prepare("SELECT sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY name")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?)
        };
        if schema.is_empty() {
            conn.execute_batch(&format!("BEGIN;{PAGES_SCHEMA}{EPISODES_SCHEMA}COMMIT;"))?;
        } else if schema == schema_of(PAGES_SCHEMA)? {
            // An outbox written before open episodes were kept: none are open.
            conn.execute_batch(&format!("BEGIN;{EPISODES_SCHEMA}COMMIT;"))?;
        } else {
            ensure!(
                schema == schema_of(&format!("{PAGES_SCHEMA}{EPISODES_SCHEMA}"))?,
                "unsupported alert outbox schema"
            );
        }
        let check: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        ensure!(check == "ok", "alert outbox integrity check failed");
        conn.pragma_update(None, "journal_mode", "DELETE")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let store = Self { conn, cfg, metrics };
        let (count, bytes) = store.size()?;
        ensure!(count <= store.cfg.max_pages && bytes <= store.cfg.max_bytes, "alert outbox exceeds configured bounds");
        for (route, enabled) in [(Severity::Critical, routes[0]), (Severity::Warning, routes[1])] {
            let count: i64 =
                store.conn.query_row("SELECT count(*) FROM pages WHERE route=?", [route.as_str()], |r| r.get(0))?;
            ensure!(enabled || count == 0, "pending pages need the {} webhook route", route.as_str());
            store.metrics.alert_pages_pending.get_or_create(&SeverityLabel { severity: route.as_str() }).set(count);
        }
        let mut stmt = store.conn.prepare("SELECT id, route, body FROM pages")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            store.decode(row.get(0)?, row.get::<_, String>(1)?.as_str(), row.get::<_, String>(2)?.as_str())?;
        }
        drop(rows);
        drop(stmt);
        Ok(store)
    }

    /// Counts payload bytes and pages; the size is bounded before each insert.
    /// Error cases:
    /// - SQLite cannot read the totals
    fn size(&self) -> anyhow::Result<(u64, u64)> {
        Ok(self.conn.query_row("SELECT count(*), coalesce(sum(length(CAST(body AS BLOB))),0) FROM pages", [], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
        })?)
    }

    /// Commits one page before it becomes eligible for delivery.
    /// Error cases:
    /// - serialization, time arithmetic, or the transaction fails
    fn add(&mut self, page: Page) -> anyhow::Result<bool> {
        let severity = page.severity.as_str();
        let created_ms = now_ms();
        let expires_ms =
            created_ms.checked_add(self.cfg.max_age.as_millis() as i64).context("outbox expiry overflow")?;
        let (key, status) = (page.key.clone(), page.status);
        let body = serde_json::to_string(&Pending { id: 0, page, created_ms, expires_ms })?;
        let (count, bytes) = self.size()?;
        if count >= self.cfg.max_pages
            || body.len() > MAX_PAGE_BYTES
            || bytes.saturating_add(body.len() as u64) > self.cfg.max_bytes
        {
            self.metrics.alert_outbox_overflows.inc();
            return Ok(false);
        }
        let tx = self.conn.transaction()?;
        tx.execute("INSERT INTO pages(route,body) VALUES(?,?)", params![severity, body])?;
        match status {
            Status::Firing if key.len() <= MAX_EPISODE_KEY => {
                tx.execute("INSERT OR REPLACE INTO episodes(key,severity) VALUES(?,?)", params![key, severity])?;
                let cap = MAX_EPISODES.min(self.cfg.max_pages as usize);
                tx.execute("DELETE FROM episodes WHERE id <= (SELECT max(id) FROM episodes) - ?", [cap as i64])?;
            }
            Status::Firing => {}
            Status::Resolved => {
                tx.execute("DELETE FROM episodes WHERE key=?", [key])?;
            }
        }
        tx.commit()?;
        self.metrics.alert_pages_pending.get_or_create(&SeverityLabel { severity }).inc();
        Ok(true)
    }

    /// Fetches one route's oldest row, validating the serialized payload before use.
    /// Error cases:
    /// - SQLite, the payload, or its retention times are inconsistent
    fn next(&self, route: Severity) -> anyhow::Result<Option<Pending>> {
        let row = self
            .conn
            .query_row("SELECT id,body FROM pages WHERE route=? ORDER BY id LIMIT 1", [route.as_str()], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .optional()?;
        row.map(|(id, body)| self.decode(id, route.as_str(), &body)).transpose()
    }

    /// Checks row bounds and episode identity without treating host data as authenticated.
    /// Error cases:
    /// - row, route, key, payload size, or lifetime is invalid
    fn decode(&self, id: i64, route: &str, body: &str) -> anyhow::Result<Pending> {
        ensure!(body.len() <= MAX_PAGE_BYTES, "oversized alert outbox page");
        let mut pending: Pending = serde_json::from_str(body)?;
        ensure!(
            id > 0 && pending.id == 0 && pending.page.severity.as_str() == route,
            "invalid alert outbox route or id"
        );
        ensure!(
            pending.expires_ms > pending.created_ms
                && pending
                    .expires_ms
                    .checked_sub(pending.created_ms)
                    .is_some_and(|age| age <= self.cfg.max_age.as_millis() as i64),
            "invalid alert outbox lifetime"
        );
        ensure!(pending.created_ms <= now_ms(), "alert outbox clock moved backwards");
        let fields = pending.page.fields.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",");
        ensure!(pending.page.key == format!("{}{{{fields}}}", pending.page.alert), "invalid alert outbox episode key");
        pending.id = id;
        Ok(pending)
    }

    /// The open episodes, oldest first, checked against the bounds `add` keeps.
    /// Error cases:
    /// - SQLite fails or a row is out of bounds or names no route
    fn open_episodes(&self) -> anyhow::Result<Vec<(String, Severity)>> {
        let mut stmt = self.conn.prepare("SELECT key, severity FROM episodes ORDER BY id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut open = Vec::new();
        for row in rows {
            let (key, severity) = row?;
            let severity = match severity.as_str() {
                "critical" => Severity::Critical,
                "warning" => Severity::Warning,
                _ => return Err(anyhow!("invalid alert outbox episode severity")),
            };
            ensure!(key.len() <= MAX_EPISODE_KEY, "oversized alert outbox episode key");
            open.push((key, severity));
        }
        ensure!(open.len() <= self.episode_cap(), "alert outbox exceeds configured episode bound");
        Ok(open)
    }

    /// Most open episodes kept: the in-memory tracker's cap, and never more than the page bound.
    fn episode_cap(&self) -> usize {
        MAX_EPISODES.min(self.cfg.max_pages as usize)
    }

    /// Commits removal, updating pending metrics only after the delete succeeds.
    /// Error cases:
    /// - the id is absent or the database write fails
    fn remove(&mut self, id: i64) -> anyhow::Result<()> {
        let route: String = self.conn.query_row("SELECT route FROM pages WHERE id=?", [id], |r| r.get(0))?;
        self.conn.execute("DELETE FROM pages WHERE id=?", [id])?;
        let severity = if route == "critical" { "critical" } else { "warning" };
        self.metrics.alert_pages_pending.get_or_create(&SeverityLabel { severity }).dec();
        Ok(())
    }
}

/// Delivers a response and stops the owner on errors so its supervisor stops the node.
/// Error cases:
/// - the operation failed
fn respond<T>(reply: Reply<T>, result: anyhow::Result<T>) -> anyhow::Result<()> {
    match result {
        Ok(value) => {
            let _ = reply.send(Ok(value));
            Ok(())
        }
        Err(e) => {
            let message = format!("{e:#}");
            let _ = reply.send(Err(e));
            Err(anyhow!(message))
        }
    }
}

#[cfg(test)]
mod tests;
