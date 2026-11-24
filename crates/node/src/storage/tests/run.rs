//! The watcher loop's branches: a read that outlasts its bound stays the one in flight, and a read that panics ends
//! the loop. The bound and the interval are tens of milliseconds, on real time and a read held by a channel.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use crate::test_support::capture_alerts_and_resolutions;
use tokio::time::timeout;

fn fast() -> StorageSection {
    StorageSection { interval: Duration::from_millis(20), ..StorageSection::default() }
}

#[tokio::test]
async fn a_read_past_its_bound_is_kept_in_flight_and_its_late_result_ends_the_alert() {
    let (_logging, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let (release, gate) = mpsc::channel::<()>();
    let (gate, started) = (Mutex::new(gate), Arc::new(AtomicUsize::new(0)));
    let counted = started.clone();
    let read = move || {
        // Only the first read waits for the test.
        if counted.fetch_add(1, Ordering::SeqCst) == 0 {
            gate.lock().unwrap().recv().unwrap();
        }
        Reading::default()
    };
    let watcher = tokio::spawn(run_reads(read, fast(), Arc::new(Metrics::new()), Duration::from_millis(100)));

    let unread = timeout(Duration::from_secs(20), alerts.recv()).await.unwrap().unwrap();
    assert_eq!((unread["alert"].as_str(), unread["store"].as_str()), ("storage_unreadable", "all"));
    // Several more ticks pass with the first read still held, and none starts another.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(started.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();
    // `resolved` first holds the resolutions the first read's stores earn; the one for `all` follows the late result.
    loop {
        let line = timeout(Duration::from_secs(20), resolved.recv()).await.unwrap().unwrap();
        if line["store"] == "all" {
            assert_eq!(line["alert_resolved"], "storage_unreadable");
            break;
        }
    }
    // A later tick starts a read again once the held one has been observed.
    timeout(Duration::from_secs(20), async {
        while started.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    watcher.abort();
}

#[tokio::test]
async fn a_read_that_panics_ends_the_loop_with_an_error() {
    let watcher = tokio::spawn(run_reads(
        || -> Reading { panic!("the disk read blew up") },
        fast(),
        Arc::new(Metrics::new()),
        Duration::from_secs(5),
    ));
    let ended = timeout(Duration::from_secs(20), watcher).await.unwrap().unwrap();
    assert!(format!("{:#}", ended.unwrap_err()).contains("without a result"));
}
