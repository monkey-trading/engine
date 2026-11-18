//! The sequencer's end of the worker: the bound on the reads it forwards.

use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use super::*;

#[tokio::test]
async fn a_read_past_the_bound_is_turned_away_and_an_answered_read_frees_its_place() {
    let worker = RootWorker::start().expect("the worker starts");
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
    let deadline = Instant::now() + Duration::from_secs(60);
    while worker.shared.reads.load(Ordering::SeqCst) != MAX_READS - 1 {
        assert!(Instant::now() < deadline, "the answered read's place is freed");
        tokio::task::yield_now().await;
    }
}
