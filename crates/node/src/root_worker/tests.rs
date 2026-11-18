//! The sequencer's end of the worker: the bound on the reads it forwards, and what it reports of
//! the reads it turns away.

use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use super::*;
use crate::test_support::capture_alerts_and_resolutions;

#[tokio::test]
async fn a_read_past_the_bound_is_turned_away_and_an_answered_read_frees_its_place() {
    let mut worker = RootWorker::start(Counter::default()).expect("the worker starts");
    let account = AccountId([7; 20]);
    // As if `MAX_READS` reads waited on the worker.
    worker.shared.reads.store(MAX_READS, Ordering::SeqCst);
    let (reply, turned_away) = oneshot::channel();
    worker.read(Read::Account(account, reply)).expect("the worker runs");
    assert!(turned_away.await.is_err(), "a read past the bound is answered unavailable");
    assert_eq!(worker.shared.reads.load(Ordering::SeqCst), MAX_READS, "a read turned away takes no place");

    worker.shared.reads.store(MAX_READS - 1, Ordering::SeqCst);
    let (reply, answered) = oneshot::channel();
    worker.read(Read::Account(account, reply)).expect("the worker runs");
    assert_eq!(answered.await.expect("a read within the bound is answered"), None, "no tree is installed");
    until_freed(&worker).await;
}

#[tokio::test]
async fn each_read_turned_away_is_counted_and_an_episode_raises_one_warning_that_the_next_read_forwarded_resolves() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let refused = Counter::default();
    let mut worker = RootWorker::start(refused.clone()).expect("the worker starts");
    let read = |worker: &mut RootWorker| {
        let (reply, answer) = oneshot::channel();
        worker.read(Read::Account(AccountId([7; 20]), reply)).expect("the worker runs");
        answer
    };
    for episode in 1..=2 {
        // As if `MAX_READS` reads waited on the worker.
        worker.shared.reads.store(MAX_READS, Ordering::SeqCst);
        for _ in 0..3 {
            assert!(read(&mut worker).await.is_err(), "a read past the bound is answered unavailable");
        }
        assert_eq!(refused.get(), 3 * episode, "every read turned away is counted");
        let alert = alerts.try_recv().expect("the episode's first read turned away raises the alert");
        assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("root_reads_refused", "WARN"));
        assert!(alerts.try_recv().is_err(), "the reads after it raise no other");
        assert!(resolved.try_recv().is_err(), "nothing resolves it while reads are turned away");

        worker.shared.reads.store(MAX_READS - 1, Ordering::SeqCst);
        assert_eq!(read(&mut worker).await.expect("a read within the bound is answered"), None);
        let resolution = resolved.try_recv().expect("the next read forwarded resolves the episode");
        assert_eq!(resolution["alert_resolved"], "root_reads_refused");
        until_freed(&worker).await;
        assert_eq!(read(&mut worker).await.expect("answered"), None);
        assert!(resolved.try_recv().is_err(), "a read forwarded with no episode open resolves nothing");
        until_freed(&worker).await;
        assert_eq!(refused.get(), 3 * episode, "a read forwarded is not counted");
    }
}

/// Waits for the worker to free the place of the read it answered, leaving `MAX_READS - 1` waiting.
async fn until_freed(worker: &RootWorker) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while worker.shared.reads.load(Ordering::SeqCst) != MAX_READS - 1 {
        assert!(Instant::now() < deadline, "the answered read's place is freed");
        tokio::task::yield_now().await;
    }
}
