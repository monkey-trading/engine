//! Bounded, cross-restart reclamation authorized only by receipt membership under the trusted root.

use anyhow::{Result, ensure};
use rusqlite::{Connection, params};

use crate::authenticated::receipts::{self, RetiredRow};
use crate::authenticated::{MAX_RECEIPT_ROWS, MAX_UPDATES, Root, Store, batch, codec};

/// A caller can always offer this finite row budget to cover at least two maximum-size receipts.
pub const COMPLETE_RECEIPT_BUDGET: usize = MAX_RECEIPT_ROWS * 2;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    pub deleted: usize,
    pub receipts_consumed: usize,
    pub queued: u64,
    pub idle: bool,
    pub blocked_by_pins: bool,
    /// If nonzero, retry with at least this many retirement-row slots; no cursor was advanced.
    pub minimum_budget: usize,
    pub pages_reclaimed: u64,
}

impl Store {
    /// Returns a new live root plus bounded reclamation progress; caller must publish the new root.
    /// Error cases:
    /// - writes unauthorized, forged queue/receipt, stale root, invalid storage or failed durability
    pub fn gc(&self, root: &Root, budget: usize) -> Result<(Root, GcReport)> {
        self.gc_checked(root, budget, |_| Ok(()), || Ok(()))
    }

    /// Counts the retirement receipts queued at `root`, for tests that bound the collection backlog.
    /// Error cases:
    /// - storage, schema, or queue proof failure
    #[cfg(test)]
    pub fn pending_receipts(&self, root: &Root) -> Result<u64> {
        let mut conn = self.read_connection()?;
        let tx = conn.transaction()?;
        crate::authenticated::db::validate(&tx)?;
        let queue = receipts::queue(&tx, root)?;
        Ok(queue.hi - queue.lo)
    }

    /// Uses the same collector with private commit-boundary failure seams for regression tests.
    /// Error cases:
    /// - proof, storage, failure-hook, commit or sync checks fail
    pub(super) fn gc_checked(
        &self,
        root: &Root,
        budget: usize,
        before_commit: impl FnOnce(&Connection) -> Result<()>,
        after_commit: impl FnOnce() -> Result<()>,
    ) -> Result<(Root, GcReport)> {
        let mut report = None;
        let new_root = self.write_transaction(
            root,
            |tx| {
                let (new_root, progress) = collect(self, tx, root, budget.min(COMPLETE_RECEIPT_BUDGET))?;
                report = Some(progress);
                Ok(new_root)
            },
            before_commit,
            after_commit,
        )?;
        Ok((new_root, report.ok_or_else(|| anyhow::anyhow!("missing completed GC report"))?))
    }
}

/// Proves a bounded prefix of whole receipts and consumes enough to strictly reduce the queue.
/// Error cases:
/// - a proof/receipt/SQL/native arithmetic check fails
fn collect(store: &Store, conn: &Connection, root: &Root, budget: usize) -> Result<(Root, GcReport)> {
    let mut queue = receipts::queue(conn, root)?;
    let mut report = GcReport { queued: queue.hi - queue.lo, ..GcReport::default() };
    if root.version.is_none() {
        report.idle = true;
        return Ok((root.clone(), report));
    }
    ensure!(queue.offset == 0, "partial receipt cursors are unsupported by this format");
    let first = receipts::receipt(conn, root, queue.lo)?;
    if queue.hi - queue.lo == 1 && first.maintenance {
        report.idle = true;
        report.pages_reclaimed = vacuum(conn, budget)?;
        return Ok((root.clone(), report));
    }
    if budget == 0 {
        return Ok((root.clone(), report));
    }
    let floor = store.floor_snapshot(root)?;
    let mut retired = Vec::new();
    let mut consumed = Vec::new();
    let mut required_for_progress = 0usize;
    let start = queue.lo;
    while queue.lo < queue.hi && consumed.len() < MAX_UPDATES {
        let receipt = if queue.lo == start { first.clone() } else { receipts::receipt(conn, root, queue.lo)? };
        if !receipt.rows.is_empty() && receipt.version > floor {
            report.blocked_by_pins = true;
            break;
        }
        let needed =
            retired.len().checked_add(receipt.rows.len()).ok_or_else(|| anyhow::anyhow!("GC work overflow"))?;
        if consumed.len() < 2 {
            required_for_progress = needed;
        }
        if needed > budget {
            break;
        }
        retired.extend(receipt.rows);
        consumed.push(queue.lo);
        queue.lo += 1;
    }
    let terminal = queue.lo == queue.hi;
    // Every nonterminal commit consumes >=2 old receipts and creates exactly one new receipt.
    if consumed.is_empty() || (!terminal && consumed.len() < 2) {
        if !report.blocked_by_pins {
            report.minimum_budget = required_for_progress.max(1);
        }
        return Ok((root.clone(), report));
    }
    // A lease may have arrived during proof reads. Reserve admission under the pin registry lock.
    let since = consumed.iter().copied().filter(|v| *v != 0).max().unwrap_or(0);
    let Some(actual_floor) = store.reserve_collection(root, since)? else {
        report.blocked_by_pins = true;
        return Ok((root.clone(), report));
    };
    queue.floor = queue.floor.max(actual_floor);
    let changes = consumed
        .iter()
        .map(|version| Ok((receipts::receipt_key(&root.namespace, *version)?, None)))
        .collect::<Result<Vec<_>>>()?;
    let prepared = batch::prepare(conn, root, changes, queue.clone(), true)?;
    batch::persist(conn, &prepared)?;
    for row in &retired {
        report.deleted += match row {
            RetiredRow::Node(key) => conn.execute(
                "DELETE FROM nodes WHERE namespace=?1 AND version=?2 AND path=?3",
                params![&root.namespace[..], key.version() as i64, codec::path(key)?],
            )?,
            RetiredRow::Value { key, version } => conn.execute(
                "DELETE FROM value_rows WHERE namespace=?1 AND key=?2 AND version=?3",
                params![&root.namespace[..], &key.0[..], *version as i64],
            )?,
        };
    }
    // Physical deletion cannot invalidate either the new head or the explicitly pinned input head.
    receipts::queue(conn, &prepared.root)?;
    receipts::queue(conn, root)?;
    report.receipts_consumed = consumed.len();
    report.queued = queue.hi + 1 - queue.lo;
    report.pages_reclaimed = vacuum(conn, budget)?;
    Ok((prepared.root, report))
}

/// Reclaims at most the provided finite number of free pages using creation-time incremental mode.
/// Error cases:
/// - SQLite page accounting or bounded incremental vacuum fails
fn vacuum(conn: &Connection, pages: usize) -> Result<u64> {
    if pages == 0 {
        return Ok(0);
    }
    let before: i64 = conn.pragma_query_value(None, "page_count", |row| row.get(0))?;
    {
        // rusqlite 0.40 execute_batch stops after the first result row; this pragma yields per page.
        let mut statement = conn.prepare(&format!("PRAGMA incremental_vacuum({pages})"))?;
        let mut rows = statement.query([])?;
        let mut steps = 0usize;
        while rows.next()?.is_some() {
            steps += 1;
            ensure!(steps <= pages, "incremental vacuum exceeded its page budget");
        }
    }
    let after: i64 = conn.pragma_query_value(None, "page_count", |row| row.get(0))?;
    ensure!(after <= before, "incremental vacuum grew the database");
    ensure!(after >= 0, "negative SQLite page count");
    Ok(u64::try_from(before - after)?)
}
