use super::*;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;
use std::time::Duration;

use engine_core::Fixed;
use engine_core::command::{Command, Sequenced};
use engine_core::types::Millis;
use rusqlite::Connection;
use tokio::sync::{Semaphore, broadcast};
use tokio::time::timeout;

use crate::api::MAX_BACKFILLS;
use crate::config::HistorySection;
use crate::log::{CommandEntry, MANIFEST, Manifest, SegmentedLog};
use crate::sequencer::StreamMsg;
use crate::test_support::{
    RETENTION, SEGMENT_COMMANDS, TRADER, TestHistory, TestSequencer, assert_a_dropped_read_keeps_its_permit, deposit,
    every_event, log_entries, logged_history, open_history, published_manifest, scratch_dir, withdraw,
};

mod rebuild;
mod recovery;

const A: AccountId = AccountId([0xa; 20]);
const B: AccountId = AccountId([0xb; 20]);

#[tokio::test]
async fn pages_run_newest_first_between_exclusive_cursors() {
    let reader = sample_index("pages");
    let all = every_page(&reader, A, None, 10).await;
    assert_eq!(cursors(&all), ["13:0", "11:1", "10:2", "10:0"]);
    assert_eq!(all[0].ts, 1_300);

    let first = page(&reader, A, None, None, None, 2).await;
    assert_eq!(cursors(&first.events), ["13:0", "11:1"]);
    assert_eq!(first.next, Some(at(11, 1)));
    let second = page(&reader, A, None, first.next, None, 2).await;
    assert_eq!(cursors(&second.events), ["10:2", "10:0"]);
    assert_eq!(second.next, None);

    let between = page(&reader, A, None, Some(at(13, 0)), Some(at(10, 0)), 10).await;
    assert_eq!(cursors(&between.events), ["11:1", "10:2"]);
    assert!(page(&reader, A, None, Some(at(10, 0)), None, 10).await.events.is_empty());
    // A cursor past every seq SQLite can hold bounds like the largest one.
    let past = Some(at(u64::MAX, 0));
    assert_eq!(page(&reader, A, None, past, None, 10).await.events, all);
    assert!(page(&reader, A, None, None, past, 10).await.events.is_empty());
    assert_eq!(cursors(&every_page(&reader, B, None, 1).await), ["10:1"]);
    assert!(every_page(&reader, AccountId([0xc; 20]), None, 1).await.is_empty());
}

#[tokio::test]
async fn a_kind_filter_returns_only_events_of_that_kind() {
    let reader = sample_index("kinds");
    assert_eq!(cursors(&every_page(&reader, A, Some("deposited"), 1).await), ["13:0", "10:0"]);
    assert_eq!(cursors(&every_page(&reader, A, Some("financing"), 1).await), ["11:1"]);
    let before = page(&reader, A, Some("deposited"), Some(at(13, 0)), None, 1).await;
    assert_eq!(cursors(&before.events), ["10:0"]);
    assert_eq!(before.next, None);
    assert!(every_page(&reader, A, Some("fill"), 1).await.is_empty());
}

#[test]
fn account_of_and_kinds_cover_every_event_that_names_an_account() {
    let mut kinds = BTreeSet::new();
    for event in every_event() {
        let json = serde_json::to_value(&event).unwrap();
        let named = json
            .get("account")
            .or_else(|| json.pointer("/receipt/beneficiary/account"))
            .map(|account| account.as_str().unwrap().to_owned());
        assert_eq!(account_of(&event).map(|account| account.to_string()), named, "{json}");
        if named.is_some() {
            kinds.insert(json["type"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(kinds, KINDS.iter().map(|kind| kind.to_string()).collect());
}

/// Authenticated history stores a kind's position as its tag, so a released kind never moves.
#[test]
fn released_kinds_keep_their_authenticated_storage_tags() {
    let released = [
        "deposited",
        "deposit_rejected",
        "withdrawal_approved",
        "withdrawal_rejected",
        "order_accepted",
        "order_rejected",
        "order_cancelled",
        "fill",
        "close_out",
        "bad_debt",
        "financing",
        "emission_allocated",
    ];
    assert_eq!(KINDS[..released.len()], released);
}

#[tokio::test]
async fn custody_receipts_are_indexed_for_the_trader_and_filtered_by_kind() {
    let account = AccountId([1; 20]);
    let events: Vec<_> = every_event()
        .into_iter()
        .filter(|event| matches!(event, Event::CustodyDepositChanged { .. } | Event::CustodyDepositRejected { .. }))
        .collect();
    let path = file(&index_dir("custody-receipts"));
    Writer::open(&path, ms(RETENTION)).unwrap().0.write(&[command(10, 1_000, events.clone())]).unwrap();
    let reader = Reader::new(path, ready());
    let all = every_page(&reader, account, None, 10).await;
    assert_eq!(all.len(), 2);
    for kind in ["custody_deposit_changed", "custody_deposit_rejected"] {
        let page = every_page(&reader, account, Some(kind), 10).await;
        let expected: Vec<_> =
            all.iter().filter(|event| serde_json::to_value(&event.event).unwrap()["type"] == kind).collect();
        assert_eq!(page.iter().collect::<Vec<_>>(), expected);
        assert_eq!(page.len(), 1);
    }
    assert!(every_page(&reader, AccountId([2; 20]), None, 10).await.is_empty());
}

#[tokio::test]
async fn taking_the_same_commands_again_changes_nothing() {
    let path = file(&index_dir("idempotent"));
    let mut writer = Writer::open(&path, ms(RETENTION)).unwrap().0;
    writer.write(&sample_log()).unwrap();
    let reader = Reader::new(path.clone(), ready());
    let once = every_page(&reader, A, None, 10).await;
    writer.write(&sample_log()).unwrap();
    writer.write(&sample_log()[1..]).unwrap();
    assert_eq!(every_page(&reader, A, None, 10).await, once);
    assert_eq!(Writer::open(&path, ms(RETENTION)).unwrap().1.unwrap().command.sequenced.seq, 13);
}

#[tokio::test]
async fn events_older_than_the_retention_leave_the_index() {
    let path = file(&index_dir("retention"));
    let mut writer = Writer::open(&path, 1_000).unwrap().0;
    let deposit_a = || vec![Event::Deposited { account: A, amount: Fixed::from_int(1) }];
    writer.write(&[command(1, 0, deposit_a()), command(2, 500, deposit_a())]).unwrap();
    let reader = Reader::new(path, ready());
    assert_eq!(cursors(&every_page(&reader, A, None, 10).await), ["2:0", "1:0"]);
    // The horizon is 1_500 - 1_000: the event at 0 leaves, the one at exactly 500 stays.
    writer.write(&[command(3, 1_500, deposit_a())]).unwrap();
    assert_eq!(cursors(&every_page(&reader, A, None, 10).await), ["3:0", "2:0"]);
}

#[tokio::test]
async fn the_index_follows_the_log_and_resumes_after_a_restart() {
    let (dir, index) = (scratch_dir("history-restart"), index_dir("restart"));
    let node = TestSequencer::start_legacy(&dir, 1024);
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    node.submit(deposit(100, "dep-1")).await;
    node.submit(withdraw(40)).await;
    node.stop().await;
    history.finish().await.unwrap();
    let reader = Reader::new(file(&index), ready());
    let first = every_page(&reader, TRADER, None, 1).await;
    assert_eq!(first, logged_history(&dir, TRADER));
    // The deposit, then the withdrawal's reservation, approval, and queue update.
    assert_eq!(first.len(), 4);

    let last = last_command_seq(&dir);
    let node = TestSequencer::start_legacy(&dir, 1024);
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    assert_eq!(history.resume_at, Offset::start_of(last + 1), "the index resumes after the last command it took");
    node.submit(withdraw(10)).await;
    node.stop().await;
    history.finish().await.unwrap();
    let second = every_page(&reader, TRADER, None, 1).await;
    assert_eq!(second, logged_history(&dir, TRADER));
    assert_eq!(second.len(), 7);
    assert_eq!(second[3..], first[..]);
}

#[tokio::test]
async fn a_removed_index_is_rebuilt_from_the_log_segments() {
    let (dir, index) = (scratch_dir("history-rebuild"), index_dir("rebuild"));
    let node = TestSequencer::start_legacy(&dir, 1024);
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    for n in 0..10 {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    node.stop().await;
    history.finish().await.unwrap();
    let reader = Reader::new(file(&index), ready());
    let served = every_page(&reader, TRADER, None, 3).await;
    assert_eq!(served.len(), 10);

    fs::remove_dir_all(&index).unwrap();
    let node = TestSequencer::start_legacy(&dir, 1024);
    assert!(node.manifest.borrow().segments.len() > 2, "the log spans several completed segments");
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    assert_eq!(history.resume_at, Offset::start_of(0));
    node.stop().await;
    history.finish().await.unwrap();
    assert_eq!(every_page(&reader, TRADER, None, 3).await, served);
}

#[tokio::test]
async fn the_sequencer_runs_on_while_the_index_writer_waits_on_its_file() {
    let (dir, index) = (scratch_dir("history-stall"), index_dir("stall"));
    let node = TestSequencer::start_legacy(&dir, 1024);
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    let lock = Connection::open(file(&index)).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let sequenced = async {
        for n in 0..50 {
            node.submit(deposit(1, &format!("dep-{n}"))).await;
        }
    };
    // Each submit is its own batch, and each batch syncs the log to disk, which macOS makes a full
    // disk flush; a sequencer waiting on the index would never finish at all.
    timeout(Duration::from_secs(30), sequenced).await.expect("the sequencer does not wait on the index");
    let reader = Reader::new(file(&index), ready());
    assert!(every_page(&reader, TRADER, None, 200).await.is_empty(), "the writer took nothing while blocked");
    lock.execute_batch("ROLLBACK").unwrap();
    node.stop().await;
    history.finish().await.unwrap();
    assert_eq!(every_page(&reader, TRADER, None, 200).await.len(), 50);
}

#[tokio::test]
async fn a_dropped_backfill_keeps_its_permit_until_its_read_ends() {
    // A segment that is a pipe: opening it blocks until something opens its other end.
    let dir = scratch_dir("history-backfill-permit");
    fs::create_dir_all(&dir).unwrap();
    let segment = dir.join("00000000000000000000.jsonl");
    let (tx, manifest) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(4, tx).unwrap();
    writer
        .append(&LogEntry::Command(CommandEntry {
            seal: None,
            sequenced: Sequenced { seq: 0, ts: 0, cmd: Command::Tick },
            events: Some(Vec::new()),
        }))
        .unwrap();
    writer.flush().unwrap();
    drop(writer);
    fs::remove_file(&segment).unwrap();
    assert!(Process::new("mkfifo").arg(&segment).status().unwrap().success());
    let permits = Arc::new(Semaphore::new(MAX_BACKFILLS));
    let (live, subscriber) = broadcast::channel(1);
    let mut feed = Feed::new(subscriber, dir.clone(), manifest, permits.clone(), Some(Offset::start_of(0)));
    let release = || drop(OpenOptions::new().write(true).open(&segment).unwrap());
    assert_a_dropped_read_keeps_its_permit(&permits, feed.next(), release).await;
    drop(live);
}

// ── Helpers ──

/// A fresh index directory for one test.
fn index_dir(name: &str) -> PathBuf {
    scratch_dir(&format!("history-index-{name}"))
}

/// The index file in the index directory `index`.
fn file(index: &Path) -> PathBuf {
    index.join(INDEX_FILE)
}

/// A ready flag already set, for readers of an index no writer is catching up.
fn ready() -> watch::Receiver<bool> {
    watch::channel(true).1
}

/// Runs an index in `index` under `retention` over the whole log in `dir`, with no live messages
/// after it, and returns where it resumed.
async fn index_to_end(dir: &Path, index: &Path, retention: Duration) -> Offset {
    let mut history = open_history(&HistorySection { dir: index.to_owned(), retention }, dir).unwrap();
    history.catch_up_to(None);
    let resume_at = history.resume_at();
    follow_to_end(history, dir).await;
    resume_at
}

/// Runs `history` from where it resumes over the rest of the log in `dir`, with no live messages
/// after it.
async fn follow_to_end(history: History, dir: &Path) {
    let (live, subscriber) = broadcast::channel(1);
    drop(live);
    let (manifest, backfills) = (published_manifest(dir), Arc::new(Semaphore::new(MAX_BACKFILLS)));
    let feed = Feed::new(subscriber, dir.to_owned(), manifest, backfills, Some(history.resume_at()));
    timeout(Duration::from_secs(10), history.run(feed)).await.unwrap().unwrap();
}

/// Empties the segment the log in `dir` is still writing, which a power loss does to whatever the
/// node never synced: completed segments are synced before they are listed.
fn lose_log_tail(dir: &Path) {
    let segments = fs::read_dir(dir).unwrap().map(|file| file.unwrap().file_name().into_string().unwrap());
    let last = segments.filter(|name| name.ends_with(".jsonl") && name != MANIFEST).max().expect("a segment");
    assert!(Manifest::load(dir).unwrap().segments.iter().all(|listed| listed.file != last), "{last} is listed");
    fs::write(dir.join(last), b"").unwrap();
}

/// Writes a log in `dir` of `count` commands, seq `n` stamped `n` seconds in, each depositing to `A`,
/// and returns its entries.
fn deposit_log(dir: &Path, count: u64) -> Vec<LogEntry> {
    let events = || Some(vec![Event::Deposited { account: A, amount: Fixed::from_int(1) }]);
    let entries: Vec<LogEntry> = (0..count)
        .map(|seq| {
            let sequenced = Sequenced { seq, ts: Millis::try_from(seq).unwrap() * 1_000, cmd: Command::Tick };
            LogEntry::Command(CommandEntry { seal: None, sequenced, events: events() })
        })
        .collect();
    let mut log = SegmentedLog::open(dir, |_| Ok(()))
        .unwrap()
        .into_writer(SEGMENT_COMMANDS, watch::channel(Manifest::default()).0)
        .unwrap();
    for entry in &entries {
        log.append(entry).unwrap();
    }
    log.flush().unwrap();
    entries
}

/// Appends `count` commands to the log in `dir`, or starts one there, each depositing `amount` to `A`
/// and stamped half a second after `deposit_log` stamps its seq, so none matches a command it wrote.
fn append_deposits(dir: &Path, amount: i64, count: u64) {
    let mut next = 0;
    let log = SegmentedLog::open(dir, |entry| {
        next = command_seq(&entry).map_or(next, |seq| seq + 1);
        Ok(())
    })
    .unwrap();
    let mut writer = log.into_writer(SEGMENT_COMMANDS, watch::channel(Manifest::default()).0).unwrap();
    for seq in next..next + count {
        let ts = Millis::try_from(seq).unwrap() * 1_000 + 500;
        let events = Some(vec![Event::Deposited { account: A, amount: Fixed::from_int(amount) }]);
        let sequenced = Sequenced { seq, ts, cmd: Command::Tick };
        writer.append(&LogEntry::Command(CommandEntry { seal: None, sequenced, events })).unwrap();
    }
    writer.flush().unwrap();
}

/// The seq of the last command in the log in `dir`.
fn last_command_seq(dir: &Path) -> u64 {
    log_entries(dir).iter().rev().find_map(command_seq).expect("the log holds a command")
}

/// The seq of `entry`, if it is a command.
fn command_seq(entry: &LogEntry) -> Option<u64> {
    match entry {
        LogEntry::Command(command) => Some(command.sequenced.seq),
        _ => None,
    }
}

fn ms(duration: Duration) -> Millis {
    Millis::try_from(duration.as_millis()).unwrap()
}

fn at(seq: u64, index: u32) -> Offset {
    Offset { seq, index }
}

/// A command message at `seq`, stamped `ts`, that produced `events`.
fn command(seq: u64, ts: Millis, events: Vec<Event>) -> Arc<StreamMsg> {
    let sequenced = Sequenced { seq, ts, cmd: Command::Tick };
    Arc::new(StreamMsg::new(
        Offset::start_of(seq),
        LogEntry::Command(CommandEntry { seal: None, sequenced, events: Some(events) }),
    ))
}

/// Commands with events for `A` at 10:0, 10:2, 11:1, and 13:0, and for `B` at 10:1.
fn sample_log() -> Vec<Arc<StreamMsg>> {
    let amount = Fixed::from_int(1);
    vec![
        command(
            10,
            1_000,
            vec![
                Event::Deposited { account: A, amount },
                Event::Deposited { account: B, amount },
                Event::WithdrawalApproved { account: A, amount, nonce: 1 },
            ],
        ),
        command(11, 1_100, vec![Event::MarketOpened { market: 1 }, Event::Financing { account: A, market: 1, amount }]),
        command(12, 1_200, Vec::new()),
        command(13, 1_300, vec![Event::Deposited { account: A, amount }]),
    ]
}

/// A reader of an index that took `sample_log`.
fn sample_index(name: &str) -> Reader {
    let path = file(&index_dir(name));
    Writer::open(&path, ms(RETENTION)).unwrap().0.write(&sample_log()).unwrap();
    Reader::new(path, ready())
}

/// Runs a node on `dir` with an index in `index` through `deposits` deposits, then stops both.
async fn indexed_log(dir: &Path, index: &Path, deposits: usize) {
    let node = TestSequencer::start_legacy(dir, 1024);
    let history = TestHistory::start(&node, dir, index).await.unwrap();
    for n in 0..deposits {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    node.stop().await;
    history.finish().await.unwrap();
}

async fn page(
    reader: &Reader,
    account: AccountId,
    kind: Option<&str>,
    before: Option<Offset>,
    after: Option<Offset>,
    limit: usize,
) -> Page {
    let query = PageQuery { account, kind: kind.map(str::to_owned), before, after, limit };
    reader.page(query).await.unwrap().expect("the index is caught up")
}

/// Every page from the newest, following `next`. Each page must hold only events older than the
/// `before` it was asked for, so a page that does not move on fails rather than repeats.
async fn every_page(reader: &Reader, account: AccountId, kind: Option<&str>, limit: usize) -> Vec<HistoryEvent> {
    let (mut events, mut before): (Vec<HistoryEvent>, Option<Offset>) = (Vec::new(), None);
    loop {
        let page = page(reader, account, kind, before, None, limit).await;
        assert!(page.events.len() <= limit);
        let older = |cursor: Offset| before.is_none_or(|before| cursor < before);
        assert!(
            page.events.iter().all(|e| older(e.cursor)) && page.next.is_none_or(older),
            "{page:?} after {before:?}"
        );
        events.extend(page.events);
        match page.next {
            Some(next) => before = Some(next),
            None => return events,
        }
    }
}

fn cursors(events: &[HistoryEvent]) -> Vec<String> {
    events.iter().map(|event| event.cursor.to_string()).collect()
}
