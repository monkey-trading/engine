use super::*;

#[tokio::test(start_paused = true)]
async fn the_time_refresher_samples_every_second_and_ends_with_the_queue() {
    let (tx, mut rx) = mpsc::channel(8);
    let refresher = tokio::spawn(keep_time_fresh(tx.downgrade()));
    for _ in 0..3 {
        let sample = timeout(Duration::from_secs(2), rx.recv()).await.expect("a sample arrives each second");
        assert!(matches!(sample, Some(Request::TimeSample(_))));
    }
    assert!(rx.try_recv().is_err(), "one sample per second, no more");
    drop(tx);
    refresher.await.unwrap();
}
