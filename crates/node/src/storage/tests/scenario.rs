//! The dev scenario for the disk alerts, on real files and a real volume: a node's stores are written to a temporary
//! directory, the watcher runs against them with a free-share threshold above the volume's free space, and the
//! alert, the metrics and the unreadable-store alert and its resolution are read as an operator reads them.

use super::*;

use std::sync::Arc;

use crate::test_support::Alert;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);

#[tokio::test]
async fn dev_scenario_a_volume_past_its_threshold_alerts_and_an_unreadable_store_pages_and_resolves() {
    let (_logging, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let root = scratch_dir("storage-dev-scenario");
    let dirs = Dirs::new(&root);
    write(&dirs.log.join("segment-0.jsonl"), 3 << 20);
    write(&dirs.history.join(INDEX_FILE), 2 << 20);
    write(&dirs.history.join(MARKS_FILE), 1 << 20);
    write(&dirs.snapshots.join("cache-1.bin"), 1 << 20);
    write(&dirs.daily.join("2026-11-23.snapshot"), 4 << 20);
    write(&dirs.outbox.join("outbox.sqlite"), 1 << 18);

    // Every volume has less than all of itself free, so a warning share of 100% is above any free space; the fill
    // windows are tight enough that no growth the disk shows meanwhile projects to one.
    // An interval under a second is for this test: the section refuses it in a config.
    let cfg = StorageSection {
        interval: Duration::from_millis(50),
        warn_free_percent: 100,
        critical_free_percent: 1,
        warn_fill_within: Duration::from_secs(2),
        critical_fill_within: Duration::from_secs(1),
        rate_window: Duration::from_millis(300),
    };
    let metrics = Arc::new(Metrics::new());
    let watcher = tokio::spawn(run(dirs.layout(), cfg, metrics.clone()));

    let low = next(&mut alerts).await;
    assert_eq!(low["alert"], "disk_space_low");
    assert_eq!(low["level"], "WARN");
    let volume = "log+history_index+marks_cache+recovery_caches+daily_snapshots+alert_outbox";
    assert_eq!(low["volume"], volume);
    assert!(low["message"].contains("free of"), "{low:?}");
    // The stores' bytes and the volume's space, as the operator's Prometheus reads them.
    let text = metrics.encode();
    for (store, written) in
        [("log", 3 << 20), ("history_index", 2 << 20), ("marks_cache", 1 << 20), ("daily_snapshots", 4 << 20)]
    {
        let line =
            text.lines().find(|line| line.starts_with(&format!("engine_store_bytes{{store=\"{store}\"}} "))).unwrap();
        let bytes: u64 = line.rsplit(' ').next().unwrap().parse().unwrap();
        assert!(bytes >= written, "{line}");
    }
    assert!(text.contains(&format!("engine_volume_free_bytes{{volume=\"{volume}\"}} ")), "{text}");
    assert!(text.contains(&format!("engine_volume_total_bytes{{volume=\"{volume}\"}} ")), "{text}");

    // The daily snapshots directory stops listing: the store pages, and resolves once it lists again.
    fs::remove_dir_all(&dirs.daily).unwrap();
    write(&dirs.daily, 1);
    let unreadable = next(&mut alerts).await;
    assert_eq!((unreadable["alert"].as_str(), unreadable["store"].as_str()), ("storage_unreadable", "daily_snapshots"));
    fs::remove_file(&dirs.daily).unwrap();
    fs::create_dir(&dirs.daily).unwrap();
    let resolution = next(&mut resolved).await;
    assert_eq!(
        (resolution["alert_resolved"].as_str(), resolution["store"].as_str()),
        ("storage_unreadable", "daily_snapshots")
    );
    watcher.abort();
}

async fn next(alerts: &mut UnboundedReceiver<Alert>) -> Alert {
    timeout(WAIT, alerts.recv()).await.expect("the alert did not arrive").expect("the alert collector closed")
}
