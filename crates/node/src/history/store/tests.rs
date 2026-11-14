//! The history index store: schema checks, integrity, retention and reads.

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
