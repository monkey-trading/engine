use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tracing::warn;

use super::{ENCODE_WAIT, Encoding, Endpoint, STOP_POLL, WriteAlert};
use crate::alerts::capture;

/// A stored capture resolves the episode open when it started completing, but not a failure
/// raised while it completed, such as the shutdown wait's: that one stays open for the next.
#[test]
fn a_stored_capture_resolves_only_failures_raised_before_its_take() {
    let alert = WriteAlert(std::sync::Arc::new(std::sync::Mutex::new(true)));
    let lines = capture(|| {
        let taken = alert.take();
        alert.failed(|| warn!(alert = "snapshot_write_failed", "raised while a capture completes"));
        alert.stored(taken);
        let taken = alert.take();
        alert.stored(taken);
        let taken = alert.take();
        alert.stored(taken);
    });
    let firing: Vec<_> = lines.iter().map(|line| line.severity.is_some()).collect();
    assert_eq!(firing, [true, false], "one failure, then one resolve by the next stored capture: {lines:#?}");
}

/// An engine due from the stats worker is taken when it comes, after several polls too, and a wait
/// fails, never hangs, when the worker stops without it, refuses it, or it never comes, or once the
/// history owner is stopped, which the wait asks at the end of each poll.
#[test]
fn a_pending_encoding_is_taken_when_it_comes_and_fails_when_it_does_not() {
    let within = Duration::from_millis(50);
    let running = || false;
    let pending = || std::sync::mpsc::sync_channel::<crate::stats::background::Encoded>(1);
    assert_eq!(Encoding::Encoded("{}".into()).wait(within, running).unwrap(), "{}");
    let (reply, encoded) = pending();
    reply.send(Ok("{\"engine\":1}".into())).unwrap();
    assert_eq!(Encoding::Pending(encoded).wait(within, running).unwrap(), "{\"engine\":1}");
    let (reply, encoded) = pending();
    reply.send(Err("the copy is at None".into())).unwrap();
    let refused = Encoding::Pending(encoded).wait(within, running).unwrap_err();
    assert!(format!("{refused:#}").contains("the copy is at None"), "{refused:#}");
    let (reply, encoded) = pending();
    drop(reply);
    let stopped = Encoding::Pending(encoded).wait(within, running).unwrap_err();
    assert!(format!("{stopped:#}").contains("stopped before encoding"), "{stopped:#}");
    // A job never delivered: the sender lives on, unsent.
    let (_reply, encoded) = pending();
    let never = Encoding::Pending(encoded).wait(within, running).unwrap_err();
    assert!(format!("{never:#}").contains("within"), "{never:#}");
    // An encoding that comes after several polls is taken.
    let (reply, encoded) = pending();
    let late = std::thread::spawn(move || {
        std::thread::sleep(STOP_POLL * 3);
        reply.send(Ok("{\"engine\":2}".into())).unwrap();
    });
    assert_eq!(Encoding::Pending(encoded).wait(ENCODE_WAIT, running).unwrap(), "{\"engine\":2}");
    late.join().unwrap();
    // An owner stopped while the wait polls for a job never delivered: the stop comes after the first
    // poll ends, and the wait leaves at the end of the second, not at its bound.
    let (_reply, encoded) = pending();
    let polls = std::sync::atomic::AtomicUsize::new(0);
    let stopped_after_one = || polls.fetch_add(1, Ordering::AcqRel) >= 1;
    let started = Instant::now();
    let error = Encoding::Pending(encoded).wait(Duration::from_secs(10), stopped_after_one).unwrap_err();
    assert!(format!("{error:#}").contains("history owner stopped"), "{error:#}");
    assert_eq!(polls.load(Ordering::Acquire), 2, "the wait asked for the stop at each poll's end");
    assert!(started.elapsed() < STOP_POLL * 20, "two polls took {:?}, not one poll's length each", started.elapsed());
}

/// A shutdown capture wait ends by cancellation when it times out, or by abort on task drop;
/// either way the test-only `waiting` count returns to zero, so later assertions never see a
/// leaked waiter after the shutdown path ran.
#[tokio::test]
async fn a_dropped_idle_wait_releases_its_waiter_count() {
    let (seals, _seals) = tokio::sync::mpsc::channel(1);
    let endpoint = Endpoint::new(seals, crate::snapshot::head::StoreId([0x42; 32]), Duration::ZERO);
    let in_flight = endpoint.try_permit().expect("no capture is in flight");
    let deadline = Instant::now() + Duration::from_secs(5);

    let waiting = tokio::spawn({
        let endpoint = endpoint.clone();
        async move { endpoint.idle().await }
    });
    while endpoint.waiting.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline, "idle never counts its waiter");
        tokio::task::yield_now().await;
    }
    // Cancellation — how a shutdown wait ends on timeout — drops the acquire future, so the
    // count must fall before the cancelled task's join resolves.
    waiting.abort();
    let _ = waiting.await;
    assert_eq!(endpoint.waiting.load(Ordering::Acquire), 0, "a cancelled idle wait leaks its waiter");

    // The shutdown path wraps the wait in a timeout and moves on; the dropped future must
    // release the count just the same.
    tokio::time::timeout(Duration::from_millis(10), endpoint.idle()).await.expect_err("the permit stays held");
    assert_eq!(endpoint.waiting.load(Ordering::Acquire), 0, "a timed-out idle wait leaks its waiter");

    // The resolving wait still decrements exactly once.
    drop(in_flight);
    endpoint.idle().await;
    assert_eq!(endpoint.waiting.load(Ordering::Acquire), 0);
}
