//! Power losses and other damage found at startup. An index a power loss left behind the log
//! resumes; one the log or `history.retention` does not match, or a damaged file, is discarded with
//! a Warning alert and rebuilt from the start of the log, ending as a fresh index would.

use super::*;

use std::{iter, slice};

use tokio::sync::mpsc::UnboundedReceiver;

use crate::api::Feed;
use crate::log::offset_of;
use crate::test_support::{Alert, capture_alerts};

/// A change made while the node is down, to the log in the first directory or the index in the
/// second.
type Change = fn(&Path, &Path);

/// The issue's scenario through the sequencer: a power loss keeps every command the index took but
/// empties the log segment the node never synced, so the index is ahead of the log. Whether the
/// restarted node has yet to reach that seq again or has already sequenced another command there,
/// the index is discarded with a Warning, pages wait for the rebuild, and the rebuilt index serves
/// the log's history and none of the lost commands.
#[tokio::test]
async fn an_index_a_power_loss_left_ahead_of_the_log_is_rebuilt_before_it_serves() {
    // (case, deposits the restarted node sequences before the index opens, the cause named)
    let cases = [
        ("ahead of the log", 0, "the log ends before seq"),
        ("another command at its seq", 5, "the log holds another command at seq"),
    ];
    for (case, after, cause) in cases {
        let slug = case.replace(' ', "-");
        let (dir, index) = (scratch_dir(&format!("history-ahead-{slug}")), index_dir(&format!("ahead-{slug}")));
        indexed_log(&dir, &index, 6).await;
        let taken = last_command_seq(&dir);
        lose_log_tail(&dir);
        assert!(last_command_seq(&dir) < taken, "{case}: the power loss cost the log commands");

        let node = TestSequencer::start_legacy(&dir, 1024);
        for n in 0..after {
            node.submit(deposit(5, &format!("after-{n}"))).await;
        }
        let tip = node.next_seq().await.checked_sub(1);
        let (_guard, mut alerts) = capture_alerts();
        let (history, reader, resume_at, feed) = open_unstarted(&node, &dir, &index, tip);
        assert_eq!(resume_at, Offset::start_of(0), "{case}");
        assert_discarded(&mut alerts, cause, case);
        // Pages wait for the rebuild: following has not started, so nothing is served yet.
        let query = || PageQuery { account: TRADER, kind: None, before: None, after: None, limit: 200 };
        assert_eq!(reader.page(query()).await.unwrap(), None, "{case}: pages wait for the rebuild");
        let task = tokio::spawn(history.run(feed));
        node.stop().await;
        timeout(Duration::from_secs(10), task)
            .await
            .expect("the history task ends in time")
            .expect("history task")
            .unwrap();
        let served = reader.page(query()).await.unwrap().expect("rebuilt").events;
        assert_eq!(served, logged_history(&dir, TRADER), "{case}");
    }
}

/// Every way the index can stop matching the log or its config is discarded with one Warning naming
/// the cause, and the rebuild ends as a fresh index of that log, under that retention, would.
#[tokio::test]
async fn an_index_that_does_not_match_the_log_or_its_config_is_discarded_and_rebuilt() {
    let shorter = Duration::from_millis(1_500);
    // (case, what changes while the node is down, the retention it restarts under, the cause named)
    let cases: [(&str, Change, Duration, &str); 9] = [
        ("the log lost its unsynced tail", |log, _| lose_log_tail(log), RETENTION, "the log ends before seq 9"),
        (
            "the log lost its tail and took other commands there",
            |log, _| {
                lose_log_tail(log);
                append_deposits(log, 2, 3);
            },
            RETENTION,
            "the log holds another command at seq 9",
        ),
        (
            "the log was replaced by another",
            |log, _| {
                fs::remove_dir_all(log).unwrap();
                append_deposits(log, 2, 12);
            },
            RETENTION,
            "the log holds another command at seq 9",
        ),
        ("history.retention changed", |_, _| {}, shorter, "pruned under a retention of"),
        (
            "the index has the previous schema version",
            |_, index| sql(index, "PRAGMA user_version = 1"),
            RETENTION,
            "history schema 1",
        ),
        (
            "the host added a trigger",
            |_, index| sql(index, "CREATE TRIGGER t AFTER INSERT ON events BEGIN SELECT 1; END"),
            RETENTION,
            "did not create",
        ),
        (
            "the last command taken does not parse",
            |_, index| sql(index, "UPDATE progress SET command = 'x'"),
            RETENTION,
            "the last command taken",
        ),
        (
            "the retention is not a number",
            |_, index| sql(index, "UPDATE progress SET retention_ms = 'x'"),
            RETENTION,
            "reading history progress",
        ),
        (
            "the file is not a database",
            |_, index| fs::write(file(index), [0xff; 512]).unwrap(),
            RETENTION,
            "not a database",
        ),
    ];
    for (case, change, retention, cause) in cases {
        let slug = case.replace([' ', '.'], "-");
        let (log, index) = (scratch_dir(&format!("history-mismatch-{slug}")), index_dir(&format!("mismatch-{slug}")));
        deposit_log(&log, 10);
        index_to_end(&log, &index, RETENTION).await;
        change(&log, &index);
        assert_rebuilt(&log, &index, retention, cause, case).await;
    }
}

/// A power loss on a disk that does not keep SQLite's write order can tear any page of the file.
/// Whichever page it is, the index is discarded and rebuilt.
#[tokio::test]
async fn an_index_with_any_page_torn_is_discarded_and_rebuilt() {
    let (log, built) = (scratch_dir("history-torn-log"), index_dir("torn"));
    deposit_log(&log, 40);
    index_to_end(&log, &built, RETENTION).await;
    let page_size: i64 =
        Connection::open(file(&built)).unwrap().pragma_query_value(None, "page_size", |row| row.get(0)).unwrap();
    let page_size = usize::try_from(page_size).unwrap();
    let pages = fs::read(file(&built)).unwrap().len() / page_size;
    assert!(pages > 6, "the index spans several pages of each table and index: {pages}");
    for page in 0..pages {
        let index = index_dir(&format!("torn-{page}"));
        copy_dir(&built, &index);
        let mut bytes = fs::read(file(&index)).unwrap();
        bytes[page * page_size..(page + 1) * page_size].fill(0);
        fs::write(file(&index), bytes).unwrap();
        assert_rebuilt(&log, &index, RETENTION, "", &format!("page {}", page + 1)).await;
    }
}

/// A power loss that costs the index only its own last writes leaves it behind a log it still
/// matches: it resumes after the last command it kept, with no alert, and ends as a fresh index
/// would. That holds for a write-ahead log torn mid-frame, for a rebuild cut short, and for a
/// discard cut short after it removed the database file.
#[tokio::test]
async fn an_index_a_power_loss_left_behind_the_log_resumes_without_a_rebuild() {
    let log = scratch_dir("history-behind-log");
    let messages: Vec<Arc<StreamMsg>> = deposit_log(&log, 10)
        .into_iter()
        .map(|entry| Arc::new(StreamMsg::new(offset_of(None, &entry).unwrap(), entry)))
        .collect();
    let longer = Duration::from_secs(4);

    // The database file holds the first five commands, and the write-ahead log the next three, one
    // write each, until the power fails partway through writing the frame that commits the third.
    let (written, torn) = (index_dir("behind-written"), index_dir("behind-torn-wal"));
    let mut writer = Writer::open(&file(&written), ms(RETENTION)).unwrap().0;
    writer.write(&messages[..5]).unwrap();
    sql(&written, "PRAGMA wal_checkpoint(TRUNCATE)");
    for message in &messages[5..8] {
        writer.write(slice::from_ref(message)).unwrap();
    }
    fs::create_dir_all(&torn).unwrap();
    fs::copy(file(&written), file(&torn)).unwrap();
    let wal = fs::read(wal_of(&written)).unwrap();
    fs::write(wal_of(&torn), &wal[..wal.len() - 100]).unwrap();
    drop(writer);

    // A rebuild under a new retention takes the first four commands before the power fails.
    let cut = index_dir("behind-rebuild-cut");
    index_to_end(&log, &cut, RETENTION).await;
    drop(open_history(&HistorySection { dir: cut.clone(), retention: longer, marks: None }, &log).unwrap());
    Writer::open(&file(&cut), ms(longer)).unwrap().0.write(&messages[..4]).unwrap();

    // A discard removes the database file, and the power fails before it removes the write-ahead log.
    let (open, orphan) = (index_dir("behind-discard-open"), index_dir("behind-discard-cut"));
    let mut writer = Writer::open(&file(&open), ms(RETENTION)).unwrap().0;
    writer.write(&messages[..3]).unwrap();
    fs::create_dir_all(&orphan).unwrap();
    fs::copy(wal_of(&open), wal_of(&orphan)).unwrap();
    drop(writer);

    // (case, index, retention, where it resumes)
    let cases = [
        ("a write-ahead log torn mid-frame", &torn, RETENTION, 7),
        ("a rebuild cut short", &cut, longer, 4),
        ("a discard cut short", &orphan, RETENTION, 0),
    ];
    for (case, index, retention, resumes) in cases {
        let (_guard, mut alerts) = capture_alerts();
        let mut history = open_history(&HistorySection { dir: index.clone(), retention, marks: None }, &log).unwrap();
        assert_eq!(history.resume_at(), Offset::start_of(resumes), "{case}");
        assert!(discards(&mut alerts).is_empty(), "{case}");
        history.catch_up_to(None);
        follow_to_end(history, &log).await;
        let fresh = index_dir(&format!("behind-{}-fresh", case.replace(' ', "-")));
        index_to_end(&log, &fresh, retention).await;
        let served = every_page(&Reader::new(file(index), ready()), A, None, 10).await;
        assert_eq!(served, every_page(&Reader::new(file(&fresh), ready()), A, None, 10).await, "{case}");
    }
}

// ── Helpers ──

/// Opens the index in `index` against the log in `log` under `retention`, and checks that it is
/// discarded with one Warning naming `cause`, resumes at the start of the log, and ends as a fresh
/// index of that log would.
async fn assert_rebuilt(log: &Path, index: &Path, retention: Duration, cause: &str, case: &str) {
    let (_guard, mut alerts) = capture_alerts();
    let mut history = open_history(&HistorySection { dir: index.to_owned(), retention, marks: None }, log).unwrap();
    assert_eq!(history.resume_at(), Offset::start_of(0), "{case}");
    assert_discarded(&mut alerts, cause, case);
    history.catch_up_to(None);
    follow_to_end(history, log).await;
    let fresh = scratch_dir(&format!("history-rebuilt-{}", case.replace([' ', '.'], "-")));
    index_to_end(log, &fresh, retention).await;
    let served = every_page(&Reader::new(file(index), ready()), A, None, 10).await;
    assert!(!served.is_empty(), "{case}");
    assert_eq!(served, every_page(&Reader::new(file(&fresh), ready()), A, None, 10).await, "{case}");
}

/// Opens the index in `index` against the log `node` writes in `dir`, letting readers in once it
/// has taken `tip`, but does not follow the log: the caller spawns `history.run(feed)` to catch
/// up, so the test can assert what readers see before the index has taken anything.
fn open_unstarted(node: &TestSequencer, dir: &Path, index: &Path, tip: Option<u64>) -> (History, Reader, Offset, Feed) {
    let cfg = HistorySection { dir: index.to_owned(), retention: RETENTION, marks: None };
    let mut history = open_history(&cfg, dir).unwrap();
    history.catch_up_to(tip);
    let backfills = Arc::new(Semaphore::new(MAX_BACKFILLS));
    let (reader, resume_at) = (history.reader(), history.resume_at());
    let feed = Feed::new(node.stream.subscribe(), dir.to_owned(), node.manifest.clone(), backfills, Some(resume_at));
    (history, reader, resume_at, feed)
}

/// Checks that the alerts so far hold exactly one discard of the index, a Warning naming `cause`.
fn assert_discarded(alerts: &mut UnboundedReceiver<Alert>, cause: &str, case: &str) {
    let discarded = discards(alerts);
    assert_eq!(discarded.len(), 1, "{case}: {discarded:?}");
    assert_eq!(discarded[0]["level"], "WARN", "{case}");
    assert!(discarded[0]["message"].contains(cause), "{case}: {discarded:?}");
}

/// The alerts so far that discard the index.
fn discards(alerts: &mut UnboundedReceiver<Alert>) -> Vec<Alert> {
    iter::from_fn(|| alerts.try_recv().ok()).filter(|alert| alert["alert"] == "history_index_discarded").collect()
}

/// Runs `statements` on the index in `index`, as the host can.
fn sql(index: &Path, statements: &str) {
    Connection::open(file(index)).unwrap().execute_batch(statements).unwrap();
}

/// The write-ahead log beside the index file in `index`.
fn wal_of(index: &Path) -> PathBuf {
    index.join(format!("{INDEX_FILE}-wal"))
}

/// Copies every file in `from` into a new directory `to`.
fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for file in fs::read_dir(from).unwrap() {
        let file = file.unwrap();
        fs::copy(file.path(), to.join(file.file_name())).unwrap();
    }
}
