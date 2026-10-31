//! Startup checks against the log, readiness to serve, and failures.

use super::*;

#[test]
fn the_writer_refuses_another_schema_version() {
    let path = file(&index_dir("schema"));
    drop(Writer::open(&path, 1).unwrap().0);
    Connection::open(&path).unwrap().pragma_update(None, "user_version", 1).unwrap();
    let err = Writer::open(&path, 1).err().expect("refused");
    assert!(format!("{err:#}").contains("history schema 1, not 2"), "{err:#}");
}

/// Missing secondary-index entries cannot make retained events disappear from successful pages.
#[tokio::test]
async fn history_rebuilds_when_a_secondary_index_omits_events() {
    for index_name in ["events_by_account", "events_by_account_kind", "events_by_ts"] {
        let (dir, index) = (scratch_dir("history-index-omission"), index_dir("index-omission"));
        indexed_log(&dir, &index, 1).await;
        let path = file(&index);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        let root: u32 = conn
            .query_row("SELECT rootpage FROM sqlite_schema WHERE name = ?1", [index_name], |row| row.get(0))
            .unwrap();
        let page_size: u32 = conn.pragma_query_value(None, "page_size", |row| row.get(0)).unwrap();
        drop(conn);
        let mut bytes = fs::read(&path).unwrap();
        let page = ((root - 1) * page_size) as usize;
        assert_eq!(bytes[page], 10, "fixture index fits on a leaf page");
        bytes[page + 1..page + page_size as usize].fill(0);
        bytes[page + 5..page + 7].copy_from_slice(&(page_size as u16).to_be_bytes());
        fs::write(&path, bytes).unwrap();
        let conn = Connection::open(&path).unwrap();
        let cursor_integrity: String =
            conn.query_row("PRAGMA integrity_check(progress)", [], |row| row.get(0)).unwrap();
        assert_eq!(cursor_integrity, "ok");
        let retained: u32 =
            conn.query_row("SELECT count(event) FROM events NOT INDEXED", [], |row| row.get(0)).unwrap();
        assert!(retained > 0, "event data remains intact");
        drop(conn);
        let cfg = HistorySection { dir: index, retention: RETENTION, marks: None };
        let mut history = open_history(&cfg, &dir).unwrap();
        assert_eq!(history.resume_at(), Offset::start_of(0), "{index_name} must trigger rebuilding");
        history.catch_up_to(Some(last_command_seq(&dir)));
        let query = PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
        assert!(history.reader().page(query).await.unwrap().is_none(), "rebuild is unavailable until caught up");
    }
}

/// Pages wait until the index has taken every command the log held when it started: a rebuild over
/// a logged history and a restart behind the log answer `None` until then, while a restart with
/// nothing left to take serves at once.
#[tokio::test]
async fn pages_wait_until_the_index_has_taken_the_log_it_started_on() {
    // (case, deposits indexed before the restart, deposits logged while the index is down, served at once)
    for (case, indexed, while_down, at_once) in
        [("rebuild", None, 3, false), ("behind", Some(2), 3, false), ("caught-up", Some(2), 0, true)]
    {
        let (dir, index) = (scratch_dir(&format!("history-ready-{case}")), index_dir(&format!("ready-{case}")));
        let node = TestSequencer::start_legacy(&dir, 1024);
        match indexed {
            Some(deposits) => {
                let history = TestHistory::start(&node, &dir, &index).await.unwrap();
                for n in 0..deposits {
                    node.submit(deposit(1, &format!("dep-{n}"))).await;
                }
                node.stop().await;
                history.finish().await.unwrap();
            }
            None => {
                node.stop().await;
                drop(Writer::open(&file(&index), ms(RETENTION)).unwrap());
            }
        }
        let node = TestSequencer::start_legacy(&dir, 1024);
        for n in 0..while_down {
            node.submit(deposit(1, &format!("down-{n}"))).await;
        }
        let tip = node.next_seq().await.checked_sub(1);
        // The writer cannot commit while this is held, so the index cannot catch up.
        let lock = Connection::open(file(&index)).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let history = TestHistory::start_with_tip(&node, &dir, &index, tip).unwrap();
        let query = || PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
        assert_eq!(history.reader.page(query()).await.unwrap().is_some(), at_once, "{case}");
        lock.execute_batch("ROLLBACK").unwrap();
        let reader = history.reader.clone();
        node.stop().await;
        history.finish().await.unwrap();
        let page = reader.page(query()).await.unwrap().expect("caught up");
        assert_eq!(page.events, logged_history(&dir, TRADER), "{case}");
    }
}

/// The writer lets readers in once it has written a command at or past the tip it was given, and
/// never before; without a tip it leaves the flag to `catch_up_to`.
#[test]
fn the_writer_lets_readers_in_once_it_writes_the_tip() {
    // (tip, commands written, readers let in)
    for (tip, seqs, let_in) in
        [(Some(5), [3, 4], false), (Some(5), [4, 5], true), (Some(5), [6, 7], true), (None, [1, 2], false)]
    {
        let writer = Writer::open(&file(&index_dir("writer-ready")), ms(RETENTION)).unwrap().0;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        seqs.iter().for_each(|seq| tx.try_send(command(*seq, 0, Vec::new())).unwrap());
        drop(tx);
        let (ready, readers) = watch::channel(false);
        writer.run(rx, ready, tip).unwrap();
        assert_eq!(*readers.borrow(), let_in, "tip {tip:?}, wrote {seqs:?}");
    }
}

/// `catch_up_to` lets readers in at once only when the index already holds the tip, and waits when
/// it has yet to take it.
#[tokio::test]
async fn readers_wait_for_a_tip_the_index_has_not_taken() {
    let (dir, index) = (scratch_dir("history-tip"), index_dir("tip"));
    indexed_log(&dir, &index, 1).await;
    let next = last_command_seq(&dir) + 1;
    let cfg = HistorySection { dir: index.clone(), retention: RETENTION, marks: None };
    // (tip, readers let in at once)
    for (tip, at_once) in [(Some(next - 1), true), (Some(next), false), (Some(next + 1), false)] {
        let mut history = open_history(&cfg, &dir).unwrap();
        assert_eq!(history.resume_at(), Offset::start_of(next));
        history.catch_up_to(tip);
        assert_eq!(history.is_ready(), at_once, "tip {tip:?}");
    }
    // A fresh index on an empty log has nothing to take.
    let mut fresh =
        HistoryCheck::open(&HistorySection { dir: index_dir("tip-fresh"), retention: RETENTION, marks: None })
            .unwrap()
            .finish()
            .unwrap();
    fresh.catch_up_to(None);
    assert!(fresh.is_ready());
}

/// An index reopened under another retention serves what a fresh index built under that retention
/// would, whether the retention grew, shrank, or stayed.
#[tokio::test]
async fn a_reopened_index_serves_what_a_rebuild_under_its_retention_would() {
    let dir = scratch_dir("history-retention-log");
    deposit_log(&dir, 3);
    let built = Duration::from_millis(1_500);
    for (case, retention) in
        [("same", built), ("shorter", Duration::from_millis(500)), ("longer", Duration::from_secs(3))]
    {
        let index = index_dir(&format!("retention-{case}"));
        assert_eq!(index_to_end(&dir, &index, built).await, Offset::start_of(0));
        let resumed = index_to_end(&dir, &index, retention).await;
        assert_eq!(resumed, Offset::start_of(if retention == built { 3 } else { 0 }), "{case}");
        let fresh = index_dir(&format!("retention-{case}-fresh"));
        index_to_end(&dir, &fresh, retention).await;
        let served = every_page(&Reader::new(file(&index), ready()), A, None, 10).await;
        assert_eq!(served, every_page(&Reader::new(file(&fresh), ready()), A, None, 10).await, "{case}");
    }
}

/// A history failure carries its cause: an unusable index directory, an index the node must discard
/// but cannot remove, and a damaged log segment the feed meets while catching up.
#[tokio::test]
async fn a_history_failure_names_its_cause() {
    let (dir, index) = (scratch_dir("history-causes"), index_dir("causes"));
    let occupied = scratch_dir("history-causes-file");
    File::create(&occupied).unwrap();
    let cfg = HistorySection { dir: occupied.join("history"), retention: RETENTION, marks: None };
    let err = HistoryCheck::open(&cfg).err().expect("no directory under a file");
    assert!(format!("{err:#}").contains("creating"), "{err:#}");

    // An index ahead of its log, whose write-ahead log the host replaced with a directory.
    let (log, stuck) = (scratch_dir("history-causes-stuck-log"), index_dir("causes-stuck"));
    deposit_log(&log, 10);
    index_to_end(&log, &stuck, RETENTION).await;
    lose_log_tail(&log);
    fs::create_dir(stuck.join(format!("{INDEX_FILE}-wal"))).unwrap();
    let cfg = HistorySection { dir: stuck, retention: RETENTION, marks: None };
    let err = open_history(&cfg, &log).err().expect("the index cannot be discarded");
    assert!(format!("{err:#}").contains(&format!("removing {}", cfg.dir.join(INDEX_FILE).display())), "{err:#}");

    indexed_log(&dir, &index, 1).await;
    let trusted = published_manifest(&dir);
    let manifest = Manifest::load(&dir).unwrap();
    let first = &manifest.segments[0];
    OpenOptions::new().append(true).open(dir.join(&first.file)).unwrap().write_all(b"\n").unwrap();
    let fresh = HistorySection { dir: index_dir("causes-fresh"), retention: RETENTION, marks: None };
    let mut history = HistoryCheck::open(&fresh).unwrap().finish().unwrap();
    history.catch_up_to(None);
    let (live, subscriber) = broadcast::channel(1);
    drop(live);
    let backfills = Arc::new(Semaphore::new(MAX_BACKFILLS));
    let feed = Feed::new(subscriber, dir.clone(), trusted, backfills, Some(history.resume_at()));
    let err = timeout(Duration::from_secs(10), history.run(feed)).await.unwrap().expect_err("the feed fails");
    assert!(format!("{err:#}").contains("does not match its listing"), "{err:#}");
}

/// An index the host corrupted, or that SQLite cannot use, fails with its cause, which is what the
/// Warning names when the node discards it: each kind of bad row a read meets, a progress value that
/// does not decode, a file that is not SQLite or is gone, and no write-ahead log.
#[tokio::test]
async fn an_unusable_index_fails_with_its_cause() {
    let (dir, built) = (scratch_dir("history-corrupt"), index_dir("corrupt"));
    indexed_log(&dir, &built, 1).await;
    let query = || PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 10 };
    // (case, what the host changes, whether a read or the open meets it, the cause named)
    let cases = [
        ("an event that does not parse", "UPDATE events SET event = 'not json'", false, "history event"),
        ("an event time that is not a number", "UPDATE events SET ts = 'x'", false, "reading a history row"),
        ("a seq out of range", "UPDATE events SET seq = -1", false, "history row -1:0"),
        ("an index out of range", "UPDATE events SET seq = 5, idx = -1", false, "history row 5:-1"),
        ("a last command that does not parse", "UPDATE progress SET command = 'x'", true, "the last command taken"),
        (
            "a retention that is not a number",
            "UPDATE progress SET retention_ms = 'x'",
            true,
            "reading history progress",
        ),
    ];
    for (n, (case, change, at_open, cause)) in cases.into_iter().enumerate() {
        let index = index_dir(&format!("corrupt-{n}"));
        fs::create_dir_all(&index).unwrap();
        for file in fs::read_dir(&built).unwrap() {
            let file = file.unwrap();
            fs::copy(file.path(), index.join(file.file_name())).unwrap();
        }
        Connection::open(file(&index)).unwrap().execute_batch(change).unwrap();
        let err = if at_open {
            Writer::open(&file(&index), ms(RETENTION)).err().expect(case)
        } else {
            Reader::new(file(&index), ready()).page(query()).await.expect_err(case)
        };
        assert!(format!("{err:#}").contains(cause), "{case}: {err:#}");
    }

    let gone = file(&index_dir("corrupt-gone"));
    let err = Reader::new(gone, ready()).page(query()).await.expect_err("no index file");
    assert!(format!("{err:#}").contains("opening"), "{err:#}");

    let junk = file(&index_dir("corrupt-junk"));
    fs::create_dir_all(junk.parent().unwrap()).unwrap();
    fs::write(&junk, "not a SQLite database, but long enough for SQLite to read its header and say so").unwrap();
    let err = Writer::open(&junk, 1).err().expect("not a database");
    assert!(format!("{err:#}").contains("not a database"), "{err:#}");

    let err = Writer::open(Path::new(":memory:"), 1).err().expect("no write-ahead log");
    assert!(format!("{err:#}").contains("cannot use a write-ahead log"), "{err:#}");
}

#[tokio::test]
async fn a_failed_index_write_stops_the_history_task() {
    let (dir, index) = (scratch_dir("history-fail"), index_dir("fail"));
    let node = TestSequencer::start_legacy(&dir, 1024);
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    Connection::open(file(&index)).unwrap().execute_batch("DROP TABLE events").unwrap();
    node.submit(deposit(1, "dep-1")).await;
    let err = history.finish().await.expect_err("the write fails");
    assert!(format!("{err:#}").contains("did not create"), "{err:#}");
}
