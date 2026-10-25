//! Stream backfill over authenticated segments and the active prefix.

use super::*;

use std::time::Duration;

use tokio::sync::{Semaphore, broadcast};
use tokio::time::timeout;

use crate::api::Feed;
use crate::sequencer::StreamMsg;

#[tokio::test]
async fn feed_from_is_inclusive_and_after_is_exclusive_at_a_sequences_first_message() {
    let mut f = fixture("catalog-feed-bounds", 2);
    f.writer.append(&command(0)).expect("command");
    f.writer.append(&checkpoint(0)).expect("checkpoint");
    f.writer.append(&command(1)).expect("command");
    f.writer.flush().expect("flush");
    let (live, _) = broadcast::channel(8);
    for (start, expected) in [
        (Offset::start_of(0), vec![Offset::start_of(0), Offset { seq: 0, index: 1 }, Offset::start_of(1)]),
        (Offset::start_of(0).next(), vec![Offset { seq: 0, index: 1 }, Offset::start_of(1)]),
        (Offset { seq: 0, index: 1 }.next(), vec![Offset::start_of(1)]),
        (Offset::start_of(1), vec![Offset::start_of(1)]),
    ] {
        let mut feed =
            Feed::new(live.subscribe(), f.dir.clone(), f.view.clone(), Arc::new(Semaphore::new(1)), Some(start));
        let mut actual = Vec::new();
        for _ in 0..expected.len() {
            actual.push(next(&mut feed).await.offset);
        }
        assert_eq!(actual, expected);
    }
}

#[tokio::test]
async fn lagged_feed_backfills_verified_bytes_across_rotation_without_gaps_or_duplicates() {
    let mut f = fixture("catalog-feed-rotation-lag", 2);
    let (live, subscriber) = broadcast::channel(2);
    let mut feed = Feed::new(subscriber, f.dir.clone(), f.view.clone(), Arc::new(Semaphore::new(1)), None);
    publish(&mut f, &live, command(0));
    assert_eq!(next(&mut feed).await.offset, Offset::start_of(0));
    for seq in 1..10 {
        publish(&mut f, &live, command(seq));
    }
    let mut got = Vec::new();
    for _ in 1..10 {
        got.push(next(&mut feed).await.offset);
    }
    assert_eq!(got, (1..10).map(Offset::start_of).collect::<Vec<_>>());
    publish(&mut f, &live, command(10));
    assert_eq!(next(&mut feed).await.offset, Offset::start_of(10));
    assert!(f.view.borrow().segments.is_empty());
}

#[tokio::test]
async fn lagged_feed_cannot_launder_a_changed_active_file_into_history() {
    let mut f = fixture("catalog-feed-active-lag", 100);
    let (live, subscriber) = broadcast::channel(2);
    let mut feed = Feed::new(subscriber, f.dir.clone(), f.view.clone(), Arc::new(Semaphore::new(1)), None);
    publish(&mut f, &live, command(0));
    assert_eq!(next(&mut feed).await.offset, Offset::start_of(0));
    for seq in 1..10 {
        publish(&mut f, &live, command(seq));
    }
    let path = f.dir.join("00000000000000000000.jsonl");
    let text = fs::read_to_string(&path).expect("active bytes");
    let changed = text.replacen("\"ts\":1,", "\"ts\":9,", 1);
    assert_ne!(text, changed);
    fs::write(path, changed).expect("host active rewrite");
    let result = timeout(Duration::from_secs(10), feed.next()).await.expect("bounded response");
    assert!(format!("{:#}", result.expect_err("history input rejected")).contains("flushed prefix"));
}

/// Publishes only after the actual bytes and commitment have both been flushed.
fn publish(f: &mut Fixture, live: &broadcast::Sender<Arc<StreamMsg>>, entry: LogEntry) {
    let offset = f.writer.append(&entry).expect("writer append");
    f.writer.flush().expect("writer flush");
    live.send(Arc::new(StreamMsg::new(offset, entry))).expect("subscribed");
}

/// Bounds the test's wait for one exact authenticated feed message.
async fn next(feed: &mut Feed) -> Arc<StreamMsg> {
    timeout(Duration::from_secs(10), feed.next())
        .await
        .expect("feed deadline")
        .expect("verified feed")
        .expect("message")
}
