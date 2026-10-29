use super::*;
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::pin::Pin;
use std::sync::mpsc;
use std::task::{Context as TaskContext, Poll};

use engine_core::Fixed;
use engine_core::command::{Command, Sequenced};
use engine_core::event::Event;
use engine_core::types::AccountId;
use engine_wire::{FrameVerifier, SignedFrame};
use futures_util::FutureExt;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};

use crate::api::Signing;
use crate::clock::test_clock::TestClock;
use crate::log::{CommandEntry, LogEntry, SegmentedLog, segment_seq};
use crate::tee::EnclaveKey;
use crate::test_support::{TestSequencer, deposit, log_entries, log_messages, scratch_dir, trusted, withdraw};

const SEND_TIMEOUT: Duration = Duration::from_secs(1);

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
    let deposits = vec![Event::Deposited { account: AccountId([1; 20]), amount: Fixed::from_int(1) }; 200];
    writer.append(&command(1, deposits)).unwrap();
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
    // As if seq's lines were cut from disk after being published: the next live message is seq + 1.
    let tick = CommandEntry {
        seal: None,
        sequenced: Sequenced { seq: seq + 1, ts: 0, cmd: Command::Tick },
        events: Some(Vec::new()),
    };
    node.stream.send(Arc::new(StreamMsg::new(Offset::start_of(seq + 1), LogEntry::Command(tick)))).unwrap();
    let answer = timeout(Duration::from_secs(10), feed.next()).await.expect("an answer in time");
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

#[tokio::test(start_paused = true)]
async fn a_subscriber_that_stops_reading_is_closed_at_the_send_timeout() {
    let (live, feed) = live_feed(4);
    live.send(tick(0)).unwrap();
    let mut subscriber = Subscriber { stopped_reading: true, ..Subscriber::default() };
    let started = tokio::time::Instant::now();
    timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None))
        .await
        .expect("closed, not waited on");
    assert_eq!(started.elapsed(), SEND_TIMEOUT);
    assert!(subscriber.sent.is_empty());
    drop(live);
}

#[tokio::test(start_paused = true)]
async fn a_subscriber_that_disconnects_is_dropped_with_no_message_sent() {
    let (_live, feed) = live_feed(4);
    let mut subscriber = Subscriber { disconnected: true, ..Subscriber::default() };
    timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None))
        .await
        .expect("its place is freed without waiting for a message");
    assert!(subscriber.sent.is_empty());
    assert!(!subscriber.closed, "nothing is left to answer");
}

#[tokio::test(start_paused = true)]
async fn a_subscriber_close_is_answered() {
    let (_live, feed) = live_feed(4);
    let incoming = VecDeque::from([Ok(Message::Ping(vec![1].into())), Ok(Message::Close(None))]);
    let mut subscriber = Subscriber { incoming, ..Subscriber::default() };
    timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None))
        .await
        .expect("the stream ends at the subscriber's close");
    assert!(subscriber.closed, "the close reply is flushed");
}

#[tokio::test(start_paused = true)]
async fn what_a_subscriber_sends_does_not_end_its_stream() {
    let (live, feed) = live_feed(4);
    let incoming = VecDeque::from([Ok(Message::Text("hello".into())), Ok(Message::Pong(vec![].into()))]);
    let mut subscriber = Subscriber { incoming, ..Subscriber::default() };
    let open = timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None)).await;
    assert!(open.is_err(), "the stream is still open");
    assert!(subscriber.incoming.is_empty(), "what it sent was read");
    drop(live);
}

#[tokio::test(start_paused = true)]
async fn a_subscriber_read_error_ends_its_stream() {
    let (_live, feed) = live_feed(4);
    let incoming = VecDeque::from([Err(Error::new(io::Error::other("connection reset")))]);
    let mut subscriber = Subscriber { incoming, ..Subscriber::default() };
    timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None))
        .await
        .expect("the stream ends at the read error");
    assert!(!subscriber.closed);
}

#[tokio::test(start_paused = true)]
async fn every_arm_stays_live_while_another_is_busy() {
    // Input that keeps coming still lets a message published mid-flood go out before the flood ends.
    let (live, feed) = live_feed(4);
    let incoming = (0..10_000).map(|_| Ok(Message::Text("noise".into()))).collect();
    let mut subscriber = Subscriber { incoming, publish_after: Some((100, live.clone())), ..Subscriber::default() };
    let _open = timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None)).await;
    let left = subscriber.input_left_at_first_send.expect("the message mid-flood was sent");
    assert!(left > 0, "sending stays live under a flood of input");

    // A backlog of messages still lets input be read, and a close be answered, before it drains.
    for (input, answered) in [(Message::Ping(vec![1].into()), false), (Message::Close(None), true)] {
        let (live, feed) = live_feed(1024);
        for seq in 0..500 {
            live.send(tick(seq)).unwrap();
        }
        let mut subscriber = Subscriber { incoming: VecDeque::from([Ok(input.clone())]), ..Subscriber::default() };
        let _ = timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None)).await;
        let sent = subscriber.sent_when_first_read.expect("the input was read");
        assert!(sent < 500, "{input:?} read while {} messages still waited", 500 - sent);
        assert_eq!(subscriber.closed, answered, "{input:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_quiet_signed_subscriber_gets_a_heartbeat_at_once_and_then_exactly_each_interval() {
    let (live, feed) = live_feed(4);
    let interval = Duration::from_secs(5);
    let time = trusted(Arc::new(TestClock::default()));
    let signing = Signing::new(Arc::new(EnclaveKey::test_key()), 31337, AccountId([0; 20]), interval, time);
    let connection = Connection::new([7; 32], "/v1/stream?signed=1&challenge=07");
    let framing = Framing::new(Arc::new(signing), connection, watch::channel(Manifest::default()).1);
    let mut subscriber = Subscriber::default();
    let started = Instant::now();
    let _open =
        timeout(interval * 3 + Duration::from_millis(1), forward(&mut subscriber, feed, SEND_TIMEOUT, Some(framing)))
            .await;
    let offsets: Vec<Duration> = subscriber.sent_at.iter().map(|at| at.duration_since(started)).collect();
    assert_eq!(offsets, [Duration::ZERO, interval, interval * 2, interval * 3]);
    let deployment = engine_wire::Deployment { chain_id: 31337, vault: [0; 20] };
    let mut verifier = FrameVerifier::new(EnclaveKey::test_key().address(), deployment, connection);
    for sent in &subscriber.sent {
        let Message::Text(text) = sent else { panic!("{sent:?}") };
        let frame: SignedFrame = serde_json::from_str(text).unwrap();
        assert_eq!(verifier.verify(&frame), Ok(()));
        assert_eq!(frame.msg, r#"{"type":"heartbeat","head":null}"#);
    }
    drop(live);
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

/// A subscriber's socket. It records what it is sent and whether it was closed, or accepts nothing
/// once it stops reading. It sends `incoming`, then nothing until it disconnects.
#[derive(Default)]
struct Subscriber {
    sent: Vec<Message>,
    closed: bool,
    stopped_reading: bool,
    incoming: VecDeque<Result<Message, Error>>,
    disconnected: bool,
    /// How much of `incoming` was still unread when the first message was sent.
    input_left_at_first_send: Option<usize>,
    /// How many messages had been sent when the first of `incoming` was read.
    sent_when_first_read: Option<usize>,
    /// Publishes a tick on the channel once this many messages of `incoming` have been read.
    publish_after: Option<(usize, broadcast::Sender<Arc<StreamMsg>>)>,
    read: usize,
    /// When each message in `sent` was sent.
    sent_at: Vec<Instant>,
}

impl Stream for Subscriber {
    type Item = Result<Message, Error>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        match self.incoming.pop_front() {
            Some(incoming) => {
                if self.sent_when_first_read.is_none() {
                    self.sent_when_first_read = Some(self.sent.len());
                }
                self.read += 1;
                if let Some((after, live)) = &self.publish_after
                    && *after == self.read
                {
                    live.send(tick(0)).unwrap();
                }
                Poll::Ready(Some(incoming))
            }
            None if self.disconnected => Poll::Ready(None),
            None => Poll::Pending,
        }
    }
}

impl Sink<Message> for Subscriber {
    type Error = Error;

    fn poll_ready(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Result<(), Error>> {
        if self.stopped_reading { Poll::Pending } else { Poll::Ready(Ok(())) }
    }

    fn start_send(mut self: Pin<&mut Self>, msg: Message) -> Result<(), Error> {
        if self.input_left_at_first_send.is_none() {
            self.input_left_at_first_send = Some(self.incoming.len());
        }
        self.sent.push(msg);
        self.sent_at.push(Instant::now());
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Result<(), Error>> {
        self.closed = true;
        Poll::Ready(Ok(()))
    }
}

/// A live-only feed on a channel of `capacity`, and the channel's sender.
fn live_feed(capacity: usize) -> (broadcast::Sender<Arc<StreamMsg>>, Feed) {
    let (live, subscriber) = broadcast::channel(capacity);
    let manifest = watch::channel(Manifest::default()).1;
    (live, Feed::new(subscriber, PathBuf::new(), manifest, permits(), None))
}

/// A tick command at `seq`, as the stream sends it.
fn tick(seq: u64) -> Arc<StreamMsg> {
    let tick =
        CommandEntry { seal: None, sequenced: Sequenced { seq, ts: 0, cmd: Command::Tick }, events: Some(Vec::new()) };
    Arc::new(StreamMsg::new(Offset::start_of(seq), LogEntry::Command(tick)))
}

fn permits() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_BACKFILLS))
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
