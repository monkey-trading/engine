//! The stream's feed: backfill from disk and the live channel.

use super::*;
use std::fs;
use std::sync::mpsc;

use engine_core::command::{Command, Sequenced};
use futures_util::FutureExt;
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::tests::permits;
use crate::log::{CommandEntry, LogEntry, SegmentedLog, segment_seq};
use crate::test_support::{TestSequencer, deposit, log_entries, log_messages, scratch_dir, withdraw};

#[tokio::test]
async fn a_feed_from_an_offset_yields_the_log_then_live_messages_once_each() {
    let dir = scratch_dir("feed-from");
    let node = TestSequencer::start_legacy(&dir, 1024);
    node.submit(deposit(100, "dep-1")).await;
    let mut feed =
        Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), permits(), Some(Offset::start_of(0)));
    // These reach both the disk and the live channel before the feed reads either.
    node.submit(withdraw(40)).await;
    for n in 0..5 {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    let expected = log_messages(&dir, Offset::start_of(0));
    assert_eq!(take(&mut feed, expected.len()).await, expected);
    assert_next_is_the_next_command(&node, &mut feed).await;
}

#[tokio::test]
async fn a_feed_resumed_between_two_messages_of_one_seq_starts_at_the_second() {
    let dir = scratch_dir("feed-mid-seq");
    let node = TestSequencer::start_legacy(&dir, 1024);
    node.submit(deposit(100, "dep-1")).await;
    let seq = node.tick_to_checkpoint().await;
    node.submit(withdraw(40)).await;
    let after = Offset::start_of(seq);
    let mut feed =
        Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), permits(), Some(after.next()));

    let expected = log_messages(&dir, after.next());
    let got = take(&mut feed, expected.len()).await;
    assert!(got[0].starts_with(&format!(r#"{{"offset":"{seq}:1","type":"checkpoint""#)), "{}", got[0]);
    assert_eq!(got, expected);
    assert_next_is_the_next_command(&node, &mut feed).await;
}

#[tokio::test]
async fn a_feed_that_falls_behind_the_live_channel_catches_up_from_disk() {
    let dir = scratch_dir("feed-lag");
    let node = TestSequencer::start_legacy(&dir, 2);
    let mut feed = Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), permits(), None);
    node.submit(deposit(100, "dep-1")).await;
    let first = next_msg(&mut feed).await;
    for n in 0..20 {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    let expected = log_messages(&dir, first.offset.next());
    assert!(expected.len() > 2);
    assert_eq!(take(&mut feed, expected.len()).await, expected);
    assert_next_is_the_next_command(&node, &mut feed).await;
}

#[tokio::test]
async fn a_live_feed_starts_at_the_next_message_and_one_that_lags_before_it_starts_later() {
    let dir = scratch_dir("feed-live");
    let node = TestSequencer::start_legacy(&dir, 2);
    let mut feed = Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), permits(), None);
    assert_next_is_the_next_command(&node, &mut feed).await;

    let mut feed = Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), permits(), None);
    for n in 0..10 {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    let first = next_msg(&mut feed).await;
    let expected = log_messages(&dir, first.offset.next());
    assert_eq!(take(&mut feed, expected.len()).await, expected);
}

/// A batch longer than the write buffer is partly on disk while a subscriber backfills: the
/// subscriber gets what was flushed before it, and nothing of the batch from disk.
#[tokio::test]
async fn a_backfill_stops_at_the_last_flush_while_a_batch_is_written() {
    let dir = scratch_dir("feed-unflushed");
    let (manifest_tx, manifest) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(1024, manifest_tx).unwrap();
    let command = |seq, events| {
        LogEntry::Command(CommandEntry {
            seal: None,
            sequenced: Sequenced { seq, ts: 0, cmd: Command::Tick },
            events: Some(events),
        })
    };
    writer.append(&command(0, Vec::new())).unwrap();
    writer.flush().unwrap();
    writer.append(&command(1, Vec::new())).unwrap();
    // The sync thread has written the batch and not yet synced it.
    let job = writer.begin_sync(false).unwrap();
    job.write().unwrap();
    assert_eq!(log_entries(&dir).len(), 2, "the batch's command is on disk before it is flushed");

    let (live, subscriber) = broadcast::channel(16);
    let mut feed = Feed::new(subscriber, dir.clone(), manifest, permits(), Some(Offset::start_of(0)));
    assert_eq!(next_msg(&mut feed).await.offset, Offset::start_of(0));
    // With nothing more to come live, the feed ends rather than yield the unflushed command.
    drop(live);
    assert!(timeout(Duration::from_secs(10), feed.next()).await.unwrap().unwrap().is_none());
}

#[tokio::test]
async fn a_feed_backfills_past_one_batch_from_disk() {
    let dir = scratch_dir("feed-batches");
    let node = TestSequencer::start_legacy(&dir, 1024);
    for n in 0..BACKFILL_BATCH {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    let mut feed =
        Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), permits(), Some(Offset::start_of(0)));
    let expected = log_messages(&dir, Offset::start_of(0));
    assert!(expected.len() > BACKFILL_BATCH, "{}", expected.len());
    assert_eq!(take(&mut feed, expected.len()).await, expected);
    assert_next_is_the_next_command(&node, &mut feed).await;
}

/// The live channel keeps a message until every subscriber has taken it. A feed reading the disk lets go
/// of the messages sent before each read, which it takes from disk, and still yields every message once.
#[tokio::test]
async fn a_feed_reading_the_disk_holds_none_of_the_live_messages_sent_before_its_read() {
    let dir = scratch_dir("feed-releases");
    let node = TestSequencer::start_legacy(&dir, 1024);
    for n in 0..BACKFILL_BATCH {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    let mut feed =
        Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), permits(), Some(Offset::start_of(0)));
    let mut probe = node.stream.subscribe();
    for n in 0..5 {
        node.submit(deposit(1, &format!("before-{n}"))).await;
    }
    let before = held_for_the_feed(&mut probe, "before the first read");
    let mut got = take(&mut feed, 1).await;
    assert!(
        before.iter().all(|msg| Arc::strong_count(msg) == 1),
        "the channel holds messages the feed reads from disk"
    );
    // Sent while the feed is part way through the disk, and read in its next batch.
    got.extend(take(&mut feed, BACKFILL_BATCH - 1).await);
    assert!(feed.backlog.is_empty() && feed.reader.is_some(), "the next message needs another batch");
    for n in 0..5 {
        node.submit(deposit(1, &format!("between-{n}"))).await;
    }
    let between = held_for_the_feed(&mut probe, "between two reads");
    got.extend(take(&mut feed, 1).await);
    assert!(between.iter().all(|msg| Arc::strong_count(msg) == 1), "the channel holds messages sent between reads");

    let expected = log_messages(&dir, Offset::start_of(0));
    got.extend(take(&mut feed, expected.len() - got.len()).await);
    assert_eq!(got, expected);
    assert_next_is_the_next_command(&node, &mut feed).await;
}

#[tokio::test]
async fn a_backfill_reads_a_batch_only_with_a_permit_and_returns_it_after() {
    let dir = scratch_dir("feed-permits");
    let node = TestSequencer::start_legacy(&dir, 1024);
    for n in 0..BACKFILL_BATCH {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    let backfills = Arc::new(Semaphore::new(1));
    let mut feed = Feed::new(
        node.stream.subscribe(),
        dir.clone(),
        node.manifest.clone(),
        backfills.clone(),
        Some(Offset::start_of(0)),
    );
    let first = next_msg(&mut feed).await;
    assert_eq!(backfills.available_permits(), 1, "the permit is back once the batch is read");
    let mut got = take(&mut feed, BACKFILL_BATCH - 1).await;
    assert!(feed.backlog.is_empty() && feed.reader.is_some(), "the next message needs another batch");

    let held = backfills.clone().try_acquire_owned().unwrap();
    let mut waiting = Box::pin(feed.next());
    assert!((&mut waiting).now_or_never().is_none());
    drop(waiting);
    assert!(feed.reader.is_some(), "no batch starts without a permit");
    drop(held);

    let expected = log_messages(&dir, first.offset.next());
    got.extend(take(&mut feed, expected.len() - got.len()).await);
    assert_eq!(got, expected);
}

#[tokio::test]
async fn a_feed_from_a_seq_refuses_a_live_message_past_that_seq() {
    let dir = scratch_dir("feed-missing-start");
    let node = TestSequencer::start_legacy(&dir, 1024);
    let seq = node.next_seq().await;
    let mut feed =
        Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), permits(), Some(Offset::start_of(seq)));
    // Its read of the disk begins, finding nothing at seq.
    let mut answer = Box::pin(feed.next());
    assert!((&mut answer).now_or_never().is_none());
    // As if seq's lines were cut from disk after being published: the next live message is seq + 1.
    let tick = CommandEntry {
        seal: None,
        sequenced: Sequenced { seq: seq + 1, ts: 0, cmd: Command::Tick },
        events: Some(Vec::new()),
    };
    node.stream.send(Arc::new(StreamMsg::new(Offset::start_of(seq + 1), LogEntry::Command(tick)))).unwrap();
    let answer = timeout(Duration::from_secs(10), answer).await.expect("an answer in time");
    let err = answer.expect_err("the gap is refused");
    assert!(format!("{err:#}").contains("missing from the log on disk"), "{err:#}");
}

#[tokio::test]
async fn a_backfill_permit_is_held_until_its_read_ends_even_if_the_subscriber_leaves() {
    let permits = Arc::new(Semaphore::new(1));
    let (started, has_started) = oneshot::channel();
    let (finish, finished) = mpsc::channel::<()>();
    let read = read_holding(permits.clone().try_acquire_owned().unwrap(), move || {
        started.send(()).unwrap();
        finished.recv().unwrap();
    });
    // The subscriber leaves while the read runs.
    has_started.await.unwrap();
    drop(read);
    assert_eq!(permits.available_permits(), 0, "the running read holds its permit");
    finish.send(()).unwrap();
    let _freed = timeout(Duration::from_secs(10), permits.acquire()).await.expect("freed once the read ends").unwrap();
}

#[tokio::test]
async fn a_feed_refuses_to_go_live_past_history_missing_from_disk() {
    let dir = scratch_dir("feed-tail-gap");
    let node = TestSequencer::start_legacy(&dir, 1024);
    // Stop right after a rotation, so the tick below lands in the open segment.
    let mut n = 0;
    while node.manifest.borrow().segments.len() < 2 || newest_segment(&dir) + 1 != node.next_seq().await {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
        n += 1;
    }
    // The host unlinks the newest completed segment and the open one; the writer keeps its handle.
    let newest_listed = node.manifest.borrow().segments.last().unwrap().first_seq;
    let survivor = node.manifest.borrow().segments.iter().rev().nth(1).unwrap().last_seq;
    for name in fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()) {
        if segment_seq(&name).is_some_and(|seq| seq >= newest_listed) {
            fs::remove_file(dir.join(name)).unwrap();
        }
    }
    let mut feed =
        Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), permits(), Some(Offset::start_of(0)));
    node.submit(Command::Tick).await;
    loop {
        match timeout(Duration::from_secs(10), feed.next()).await.expect("an answer in time") {
            Ok(Some(msg)) => assert!(msg.offset.seq <= survivor, "{} yielded past the gap", msg.offset),
            Ok(None) => panic!("the stream closed"),
            Err(_) => break,
        }
    }
}

// ── Helpers ──

/// The first seq of the newest segment file in `dir`.
fn newest_segment(dir: &Path) -> u64 {
    let names = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap());
    names.filter_map(|name| segment_seq(&name)).max().unwrap()
}

/// What `probe` was sent since it last read, each still held by the channel for one other subscriber.
fn held_for_the_feed(probe: &mut broadcast::Receiver<Arc<StreamMsg>>, label: &str) -> Vec<Arc<StreamMsg>> {
    let mut sent = Vec::new();
    while let Ok(msg) = probe.try_recv() {
        sent.push(msg);
    }
    assert!(!sent.is_empty(), "{label}: messages were sent");
    assert!(sent.iter().all(|msg| Arc::strong_count(msg) == 2), "{label}: the channel holds each for the feed");
    sent
}

/// The feed's next message, failing the test if none comes within ten seconds.
async fn next_msg(feed: &mut Feed) -> Arc<StreamMsg> {
    let next = timeout(Duration::from_secs(10), feed.next()).await.expect("a message in time");
    next.unwrap().expect("sequencer running")
}

/// The next `n` messages, as the stream encodes them.
async fn take(feed: &mut Feed, n: usize) -> Vec<String> {
    let mut got = Vec::with_capacity(n);
    while got.len() < n {
        got.push(serde_json::to_string(&*next_msg(feed).await).unwrap());
    }
    got
}

/// Sequences a tick and checks it is the feed's next message, so nothing before it repeats.
async fn assert_next_is_the_next_command(node: &TestSequencer, feed: &mut Feed) {
    let seq = node.next_seq().await;
    node.submit(Command::Tick).await;
    assert_eq!(next_msg(feed).await.offset, Offset::start_of(seq));
}
