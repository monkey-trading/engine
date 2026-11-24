//! What `ingest_and_collection_at_scale` reads its commands from and measures: the recorded log read pass
//! after pass, and, over a connection of its own to the store's file, what each update wrote and what the
//! file holds.

use std::fs;
use std::ops::AddAssign;
use std::path::{Path, PathBuf};

use std::sync::Arc;

use engine_core::Fixed;
use engine_core::financing::FinancingRow;
use engine_core::types::{AccountId, Millis};
use rusqlite::{Connection, OpenFlags, params};

use crate::history::authenticated::Index;
use crate::history::derived::Rows;
use crate::log::{CommandEntry, LogEntry, SegmentedLog, Stop};

/// A recorded log read `count` times, pass `k`'s seqs moved on by `k` times its `commands` and its times by
/// `k` times its `span`, so the passes read as one log.
pub(super) struct Passes {
    log: PathBuf,
    pub count: u64,
    pub commands: u64,
    pub span: Millis,
}

/// A rollover's synthetic financing rows: one a position, `rows` positions over 100,000 accounts.
pub(super) struct Rollover(Rows);

/// What one step of `ingest_and_collection_at_scale` took and did.
#[derive(Default)]
pub(super) struct Step {
    pub commands: usize,
    pub updates: usize,
    pub indexing: f64,
    pub collection: f64,
    /// The longest collection call, and the longest run of calls after one update, in seconds.
    pub longest_call: f64,
    pub longest_turn: f64,
    pub deleted: usize,
    pub reclaimed: u64,
    /// Retirement receipts queued after the step's last collection call.
    pub queued: u64,
    pub proofs: Vec<f64>,
    pub written: Writes,
    /// Each update holding a rollover's rows, in seconds.
    pub rollovers: Vec<f64>,
}

/// What a range of versions wrote: nodes and their stored bytes, path keys included, and values and theirs.
#[derive(Clone, Copy, Default)]
pub(super) struct Writes {
    nodes: u64,
    node_bytes: u64,
    values: u64,
    value_bytes: u64,
}

/// A read-only connection to the store's file, apart from the store's own, read between its writes.
pub(super) struct Written {
    conn: Connection,
    namespace: [u8; 32],
}

impl Passes {
    /// The log at `log`, read `count` times; one read of it first counts its commands and their span.
    pub fn of(log: &Path, count: u64) -> Self {
        let (mut commands, mut first, mut last) = (0u64, None, 0);
        SegmentedLog::open(log, |entry| {
            if let LogEntry::Command(command) = entry {
                commands += 1;
                first.get_or_insert(command.sequenced.ts);
                last = command.sequenced.ts;
            }
            Ok(())
        })
        .expect("the log reads");
        let span = last - first.expect("the log holds a command") + 1;
        Self { log: log.to_owned(), count: count.max(1), commands, span }
    }

    /// Visits every command from seq `from`, through the passes, until `visit` stops.
    pub fn read(&self, from: u64, mut visit: impl FnMut(CommandEntry) -> anyhow::Result<()>) {
        for pass in from / self.commands..self.count {
            let (seqs, times) = (pass * self.commands, i64::try_from(pass).expect("a pass") * self.span);
            let read = SegmentedLog::open(&self.log, |entry| {
                let LogEntry::Command(mut command) = entry else { return Ok(()) };
                command.sequenced.seq += seqs;
                command.sequenced.ts += times;
                if command.sequenced.seq < from { Ok(()) } else { visit(command) }
            });
            if let Err(error) = read {
                assert!(error.is::<Stop>(), "the log reads: {error:#}");
                return;
            }
        }
    }
}

impl Rollover {
    /// `rows` positions, the account of each its ordinal's place among 100,000 and its market how many
    /// 100,000s precede it.
    pub fn new(rows: usize) -> Self {
        let rows = (0..rows)
            .map(|n| {
                let mut account = [0xaa; 20];
                account[..4].copy_from_slice(&u32::try_from(n % 100_000).expect("an account").to_be_bytes());
                let market = u32::try_from(n / 100_000).expect("a market") + 1;
                FinancingRow { account: AccountId(account), market, amount: Fixed::from_int(-1) }
            })
            .collect();
        Self(Arc::new(rows))
    }

    /// The rows, for one command's derived rows.
    pub fn rows(&self) -> Rows {
        self.0.clone()
    }
}

impl Step {
    /// Prints the step's figures, `label`led, for `index` and its store at `path`, over `wall` seconds.
    pub fn print(&self, label: &str, index: &Index, path: &Path, written: &Written, wall: f64) {
        let per_command = |secs: f64| secs * 1e6 / self.commands as f64;
        let per = |n: u64| n as f64 / self.commands as f64;
        let proof_ms = self.proofs.iter().sum::<f64>() * 1e3 / self.proofs.len().max(1) as f64;
        let head = index.head();
        let held = head.queue_hi - head.queue_lo;
        let bytes = fs::metadata(path).map_or(0, |metadata| metadata.len());
        let free = written.free_bytes();
        println!(
            "HISTORY_SCALE {label} at={} store_mb={} commands={} commands_per_s={:.0} indexing_us_per_command={:.0} \
             collection_us_per_command={:.0} ms_per_update={:.1} deleted_rows={} reclaimed_pages={} \
             head_proof_ms={proof_ms:.2} queued_receipts={} nodes_per_command={:.1} node_bytes_per_command={:.0} \
             values_per_command={:.2} value_bytes_per_command={:.0} deleted_rows_per_command={:.1} \
             events_held={held} free_mb={} bytes_per_event_held={:.0} longest_collection_call_ms={:.0} \
             longest_collection_turn_ms={:.0} rollovers={} longest_rollover_update_ms={:.0}",
            head.last_seq().unwrap_or(0),
            bytes >> 20,
            self.commands,
            self.commands as f64 / wall,
            per_command(self.indexing),
            per_command(self.collection),
            self.indexing * 1e3 / self.updates.max(1) as f64,
            self.deleted,
            self.reclaimed,
            self.queued,
            per(self.written.nodes),
            per(self.written.node_bytes),
            per(self.written.values),
            per(self.written.value_bytes),
            self.deleted as f64 / self.commands as f64,
            free >> 20,
            bytes.saturating_sub(free) as f64 / held.max(1) as f64,
            self.longest_call * 1e3,
            self.longest_turn * 1e3,
            self.rollovers.len(),
            self.rollovers.iter().fold(0f64, |longest, secs| longest.max(*secs)) * 1e3,
        );
    }
}

impl AddAssign for Writes {
    /// Adds another range's writes.
    fn add_assign(&mut self, other: Self) {
        self.nodes += other.nodes;
        self.node_bytes += other.node_bytes;
        self.values += other.values;
        self.value_bytes += other.value_bytes;
    }
}

impl Written {
    /// A read-only connection to the store at `path`, for `namespace`'s rows.
    pub fn open(path: &Path, namespace: [u8; 32]) -> Self {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("the store opens");
        Self { conn, namespace }
    }

    /// What the versions after `after` up to `upto` wrote: their nodes, and each value written beside the
    /// leaf node a write gives it at its own version.
    pub fn between(&self, after: Option<u64>, upto: Option<u64>) -> Writes {
        let (after, upto) = (after.map_or(-1, |v| v as i64), upto.map_or(-1, |v| v as i64));
        let (nodes, node_bytes) = self
            .conn
            .query_row(
                "SELECT count(*), coalesce(sum(length(data) + length(path)), 0) FROM nodes
                 WHERE space = (SELECT id FROM spaces WHERE namespace = ?1) AND version > ?2 AND version <= ?3",
                params![&self.namespace[..], after, upto],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .expect("the nodes written");
        let (values, value_bytes) = self
            .conn
            .query_row(
                "SELECT count(*), coalesce(sum(length(v.data)), 0) FROM nodes n JOIN value_rows v
                   ON v.space = n.space AND v.key = substr(n.data, 2, 32) AND v.version = n.version
                 WHERE n.space = (SELECT id FROM spaces WHERE namespace = ?1) AND n.version > ?2 AND n.version <= ?3
                   AND substr(n.data, 1, 1) = x'02'",
                params![&self.namespace[..], after, upto],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .expect("the values written");
        Writes {
            nodes: nodes as u64,
            node_bytes: node_bytes as u64,
            values: values as u64,
            value_bytes: value_bytes as u64,
        }
    }

    /// The file's bytes on its free list.
    pub fn free_bytes(&self) -> u64 {
        let pragma = |name: &str| self.conn.pragma_query_value(None, name, |row| row.get::<_, i64>(0)).expect(name);
        (pragma("freelist_count") * pragma("page_size")) as u64
    }

    /// Prints each table's and index's pages and bytes, then the values and nodes by kind.
    pub fn breakdown(&self) {
        let mut tables = self
            .conn
            .prepare(
                "SELECT name, count(*), sum(ncell), sum(payload), sum(unused), sum(pgsize) FROM dbstat GROUP BY name",
            )
            .expect("dbstat");
        let rows = tables
            .query_map([], |row| {
                Ok(format!(
                    "HISTORY_SCALE_TABLE name={} pages={} cells={} payload_bytes={} unused_bytes={} bytes={}",
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?
                ))
            })
            .expect("dbstat rows");
        rows.for_each(|row| println!("{}", row.expect("a dbstat row")));
        let kinds = |sql: &str, label: &str| {
            let mut statement = self.conn.prepare(sql).expect(label);
            let rows = statement
                .query_map([], |row| {
                    Ok(format!(
                        "{label} kind={} rows={} bytes={} key_bytes={}",
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?
                    ))
                })
                .expect(label);
            rows.for_each(|row| println!("{}", row.expect(label)));
        };
        kinds(
            r#"SELECT CASE
                 WHEN substr(data, 1, 1) = x'00' THEN CASE
                     WHEN CAST(substr(data, 2, 7) AS TEXT) = '{"row":' THEN 'queue_item'
                     WHEN CAST(substr(data, 2, 11) AS TEXT) = '{"account":' THEN 'rank'
                     WHEN CAST(substr(data, 2, 14) AS TEXT) = '{"generation":' THEN 'list'
                     WHEN CAST(substr(data, 2, 10) AS TEXT) = '{"format":' THEN 'state'
                     WHEN CAST(substr(data, 2, 8) AS TEXT) = '{"type":' THEN 'event'
                     ELSE 'other_whole' END
                 WHEN CAST(substr(data, 1, 7) AS TEXT) = '{"old":' THEN 'receipt'
                 WHEN CAST(substr(data, 1, 6) AS TEXT) = '{"lo":' THEN 'collection_queue'
                 ELSE 'other' END AS kind, count(*), sum(length(data)), count(*) * 72
             FROM value_rows GROUP BY kind"#,
            "HISTORY_SCALE_VALUES",
        );
        kinds(
            "SELECT CASE substr(data, 1, 1) WHEN x'01' THEN 'internal' WHEN x'02' THEN 'leaf' WHEN x'00' THEN 'null'
                 ELSE 'json' END AS kind, count(*), sum(length(data)), sum(length(path)) + count(*) * 40
             FROM nodes GROUP BY kind",
            "HISTORY_SCALE_NODES",
        );
    }
}
