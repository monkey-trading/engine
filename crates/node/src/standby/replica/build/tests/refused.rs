//! Daily snapshots of the formats before the one that carries the replay state, which a replica does not start from.

use super::*;

/// A held snapshot of a format before the one that carries the replay state, signed or not, is passed over without a
/// page, as a replay in ranges still reads it: the replica replays the whole copy.
#[tokio::test]
async fn a_snapshot_of_an_earlier_format_is_passed_over_without_a_page() {
    let dir = scratch_dir("build-snapshot-format");
    write_log(&dir).await;
    let formats: [(u32, &[&str]); 2] =
        [(1, &["carried", "history", "state", "signature"]), (2, &["carried", "history"])];
    for (format, dropped) in formats {
        let snapshots = scratch_dir("build-snapshot-format-daily");
        publish_middle(&dir, &snapshots, &recorded());
        let listing = read_manifest(&snapshots).unwrap().pop().unwrap();
        let path = snapshots.join(&listing.file);
        let mut written: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        written["schema"] = format.into();
        let fields = written.as_object_mut().unwrap();
        dropped.iter().for_each(|field| _ = fields.remove(*field));
        write_snapshot(&path, &written);
        let (_guard, mut alerts) = capture_alerts();
        assert!(usable(&snapshots, &dir, &[signer()].into(), u64::MAX).await.is_none(), "format {format}");
        assert!(alerts.try_recv().is_err(), "format {format} is passed over without a page");
    }
}
