//! The stream's subscribers.

use super::*;
use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use engine_core::command::{Command, Sequenced};
use engine_core::types::AccountId;
use engine_wire::{FrameVerifier, SignedFrame};
use tokio::time::{Instant, timeout};

use crate::api::Signing;
use crate::clock::test_clock::TestClock;
use crate::log::{CommandEntry, LogEntry};
use crate::tee::EnclaveKey;
use crate::test_support::trusted;

const SEND_TIMEOUT: Duration = Duration::from_secs(1);

#[tokio::test(start_paused = true)]
async fn a_subscriber_that_stops_reading_is_closed_at_the_send_timeout() {
    let (live, feed) = live_feed(4);
    live.send(tick(0)).unwrap();
    let mut subscriber = Subscriber { stopped_reading: true, ..Subscriber::default() };
    let started = tokio::time::Instant::now();
    timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None, (None, None, None)))
        .await
        .expect("closed, not waited on");
    assert_eq!(started.elapsed(), SEND_TIMEOUT);
    assert!(subscriber.sent.is_empty());
    drop(live);
}

#[tokio::test(start_paused = true)]
async fn a_live_message_is_timed_from_its_stamp_once_sent_to_the_subscriber() {
    let stamped = |seq| Arc::new(Arc::into_inner(tick(seq)).unwrap().stamped_at(std::time::Instant::now()));
    // A message read back from the log carries no stamp, and one never sent is not timed.
    for (stopped_reading, timed) in [(false, 2), (true, 0)] {
        let (live, feed) = live_feed(4);
        let sent = Histogram::new(crate::metrics::LATENCY_BUCKETS);
        let feed = feed.timed(sent.clone());
        for msg in [stamped(0), tick(1), stamped(2)] {
            live.send(msg).unwrap();
        }
        let mut subscriber = Subscriber { stopped_reading, ..Subscriber::default() };
        let _ =
            timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None, (None, None, None)))
                .await;
        assert_eq!(subscriber.sent.len(), if stopped_reading { 0 } else { 3 });
        assert_eq!(crate::metrics::latency::observed(&sent).0, timed, "stopped reading: {stopped_reading}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_subscriber_that_disconnects_is_dropped_with_no_message_sent() {
    let (_live, feed) = live_feed(4);
    let mut subscriber = Subscriber { disconnected: true, ..Subscriber::default() };
    timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None, (None, None, None)))
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
    timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None, (None, None, None)))
        .await
        .expect("the stream ends at the subscriber's close");
    assert!(subscriber.closed, "the close reply is flushed");
}

#[tokio::test(start_paused = true)]
async fn what_a_subscriber_sends_does_not_end_its_stream() {
    let (live, feed) = live_feed(4);
    let incoming = VecDeque::from([Ok(Message::Text("hello".into())), Ok(Message::Pong(vec![].into()))]);
    let mut subscriber = Subscriber { incoming, ..Subscriber::default() };
    let open =
        timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None, (None, None, None))).await;
    assert!(open.is_err(), "the stream is still open");
    assert!(subscriber.incoming.is_empty(), "what it sent was read");
    drop(live);
}

#[tokio::test(start_paused = true)]
async fn a_subscriber_read_error_ends_its_stream() {
    let (_live, feed) = live_feed(4);
    let incoming = VecDeque::from([Err(Error::new(io::Error::other("connection reset")))]);
    let mut subscriber = Subscriber { incoming, ..Subscriber::default() };
    timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None, (None, None, None)))
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
    let _open =
        timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None, (None, None, None))).await;
    let left = subscriber.input_left_at_first_send.expect("the message mid-flood was sent");
    assert!(left > 0, "sending stays live under a flood of input");

    // A backlog of messages still lets input be read, and a close be answered, before it drains.
    for (input, answered) in [(Message::Ping(vec![1].into()), false), (Message::Close(None), true)] {
        let (live, feed) = live_feed(1024);
        for seq in 0..500 {
            live.send(tick(seq)).unwrap();
        }
        let mut subscriber = Subscriber { incoming: VecDeque::from([Ok(input.clone())]), ..Subscriber::default() };
        let _ =
            timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None, (None, None, None)))
                .await;
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
    let framing = Framing::new(Arc::new(signing), connection, watch::channel(Manifest::default()).1, false);
    let mut subscriber = Subscriber::default();
    let started = Instant::now();
    let _open = timeout(
        interval * 3 + Duration::from_millis(1),
        forward(&mut subscriber, feed, SEND_TIMEOUT, Some(framing), (None, None, None)),
    )
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

// ── Helpers ──

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

/// A backfill that reaches a segment the log's local window let go under it, as a stream client's holds none, closes
/// the subscriber naming the archive, from which it reads on, and pages nothing.
#[tokio::test]
async fn a_backfill_that_reaches_a_removed_segment_closes_naming_the_archive_and_pages_nothing() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let dir = crate::test_support::scratch_dir("stream-removed");
    let node = crate::test_support::TestSequencer::start_legacy(&dir, 1024);
    for n in 0..2 * super::backfill::BACKFILL_BATCH {
        node.submit(crate::test_support::deposit(1, &format!("dep-{n}"))).await;
    }
    let start = Some(Offset::start_of(0));
    let mut feed = Feed::new(node.stream.subscribe(), dir.clone(), node.manifest.clone(), permits(), start).unheld();
    feed.next().await.unwrap().expect("the backfill's first message");
    // As the window does, the segments up to the one the backfill stands in leave the host.
    let at = feed.backlog.back().expect("a batch read ahead").offset.seq;
    let segments = crate::log::list_segments(&dir).unwrap();
    for (_, path) in segments.windows(2).filter(|pair| pair[0].0 <= at).map(|pair| &pair[0]) {
        std::fs::remove_file(path).unwrap();
    }
    let mut subscriber = Subscriber::default();
    timeout(Duration::from_secs(60), forward(&mut subscriber, feed, SEND_TIMEOUT, None, (None, None, None)))
        .await
        .expect("the stream ends");
    let Some(Message::Close(Some(frame))) = subscriber.sent.last() else { panic!("no close: {:?}", subscriber.sent) };
    assert!(frame.reason.as_str().contains("public archive"), "{frame:?}");
    let paged = std::iter::from_fn(|| alerts.try_recv().ok())
        .any(|alert| alert.get("alert").is_some_and(|name| name == "stream_backfill_failed"));
    assert!(!paged, "nothing pages");
    node.stop().await;
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

pub(super) fn permits() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_BACKFILLS))
}
