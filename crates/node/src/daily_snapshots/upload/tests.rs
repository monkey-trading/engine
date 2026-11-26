//! The daily snapshots' uploads to the archive's bucket, and the host's copies the window keeps.

use std::fs;

use super::*;
use crate::archive::stand_in::StandIn;
use crate::daily_snapshots::{DAY_MS, publish_ticks};
use crate::test_support::{capture_alerts, capture_alerts_and_resolutions, scratch_dir};

/// The stand-in deployment's snapshot prefix.
fn prefix() -> String {
    snapshots_prefix(31_337, engine_core::types::AccountId([0; 20]))
}

/// An uploader of the snapshots in `dir` to `bucket`, keeping each for a day of sequenced time, warning after `lag`.
fn uploader(bucket: &StandIn, dir: &Path, lag: Duration) -> (SnapshotUploader, Arc<Metrics>) {
    let metrics = Arc::new(Metrics::new());
    let window = Duration::from_millis(DAY_MS as u64);
    (SnapshotUploader::new((bucket.store(), prefix()), dir.to_owned(), (window, lag), metrics.clone()), metrics)
}

/// The object `listing`'s snapshot stands at in `bucket`, if any.
fn object(bucket: &StandIn, listing: &Listed) -> Option<Vec<u8>> {
    bucket.with(|b| b.objects.get(&format!("{}{}", prefix(), listing.file)).cloned())
}

/// Each published snapshot goes up as its file was written, at its key under the log's chain and vault, newest first,
/// and is confirmed by a read of it from the public origin; one confirmed is neither sent nor read again, and the
/// host keeps every snapshot within the window.
#[tokio::test]
async fn each_published_snapshot_goes_up_as_written_and_is_confirmed_by_its_read_back() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("snapshot-upload"));
    let first = publish_ticks(&dir, 3, 1_000);
    let (mut uploader, metrics) = uploader(&bucket, &dir, Duration::from_secs(3600));
    uploader.pass().await.unwrap();
    assert_eq!(object(&bucket, &first), Some(fs::read(dir.join(&first.file)).unwrap()));
    let second = publish_ticks(&dir, 5, 2_000);
    uploader.pass().await.unwrap();
    assert_eq!(object(&bucket, &second), Some(fs::read(dir.join(&second.file)).unwrap()));
    assert_eq!(bucket.with(|b| (b.created, b.read)), (2, 2), "each sent and read back once");
    uploader.pass().await.unwrap();
    assert_eq!(bucket.with(|b| (b.created, b.read)), (2, 2), "nothing again once confirmed");
    assert_eq!(metrics.snapshot_archive.confirmed.get(), 2);
    assert!(dir.join(&first.file).exists() && dir.join(&second.file).exists(), "both within the window");
}

/// A snapshot leaves the host only once it is confirmed and stamped more than the window before the newest, and the
/// newest of those stays, from which a replay of the window starts; the manifest keeps listing every one.
#[tokio::test]
async fn a_snapshot_leaves_the_host_once_confirmed_and_older_than_the_window_but_the_newest_before_it() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("snapshot-upload-window"));
    let days: Vec<Listed> = (0..4).map(|day| publish_ticks(&dir, 2 + day, day as i64 * DAY_MS + 1_000)).collect();
    let (mut uploader, metrics) = uploader(&bucket, &dir, Duration::from_secs(3600));
    bucket.with(|b| b.down = true);
    uploader.pass().await.unwrap_err();
    assert!(days.iter().all(|day| dir.join(&day.file).exists()), "nothing leaves before it is confirmed");
    bucket.with(|b| b.down = false);
    uploader.pass().await.unwrap();
    let held: Vec<bool> = days.iter().map(|day| dir.join(&day.file).exists()).collect();
    assert_eq!(held, [false, true, true, true], "day 1 is the newest stamped more than a day before day 3");
    assert_eq!(read_manifest(&dir).unwrap(), days, "the manifest lists every snapshot still");
    assert_eq!(metrics.snapshot_archive.removed.get(), 1);
    let fifth = publish_ticks(&dir, 6, 4 * DAY_MS + 1_000);
    uploader.pass().await.unwrap();
    let held: Vec<bool> = days.iter().chain([&fifth]).map(|day| dir.join(&day.file).exists()).collect();
    assert_eq!(held, [false, false, true, true, true]);
    assert!(object(&bucket, &days[0]).is_some() && object(&bucket, &days[1]).is_some(), "the bucket keeps them");
}

/// A snapshot whose object reads back as other bytes stays on the host whatever its age, pages
/// `archive_object_mismatch` naming the object, and is not tried again; the others go on.
#[tokio::test]
async fn an_object_that_reads_back_otherwise_keeps_the_hosts_copy() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("snapshot-upload-mismatch"));
    let days: Vec<Listed> = (0..4).map(|day| publish_ticks(&dir, 2 + day, day as i64 * DAY_MS + 1_000)).collect();
    let key = format!("{}{}", prefix(), days[0].file);
    bucket.with(|b| b.served.insert(key.clone(), b"{\"other\":true}".to_vec()));
    let (_guard, mut alerts) = capture_alerts();
    let (mut uploader, metrics) = uploader(&bucket, &dir, Duration::from_secs(3600));
    uploader.pass().await.unwrap();
    let alert = alerts.try_recv().expect("the mismatch is paged");
    assert_eq!((alert["alert"].as_str(), alert["object"].as_str()), ("archive_object_mismatch", key.as_str()));
    assert!(dir.join(&days[0].file).exists(), "its copy stays, though the window no longer keeps it");
    assert!(days[1..].iter().all(|day| object(&bucket, day).is_some()), "the others go up");
    let reads = bucket.with(|b| b.read);
    uploader.pass().await.unwrap();
    assert_eq!(bucket.with(|b| b.read), reads, "it is not read again");
    assert!(alerts.try_recv().is_err(), "nor paged again");
    assert_eq!(metrics.snapshot_archive.mismatches.get(), 1);
}

/// A file that differs from its listing, as a host that changed a byte, appended one or cut one off, creates no object,
/// pages `daily_snapshot_unreadable`, and stays; a file the host no longer holds is passed over.
#[tokio::test]
async fn a_file_that_differs_from_its_listing_creates_no_object() {
    for (case, change) in [("changed", 0usize), ("longer", 1), ("shorter", 2)] {
        let (bucket, dir) = (StandIn::start().await, scratch_dir(&format!("snapshot-upload-{case}")));
        let listing = publish_ticks(&dir, 3, 1_000);
        let path = dir.join(&listing.file);
        let mut bytes = fs::read(&path).unwrap();
        match change {
            0 => bytes[20] ^= 1,
            1 => bytes.push(b' '),
            _ => drop(bytes.pop()),
        }
        fs::write(&path, bytes).unwrap();
        let (_guard, mut alerts) = capture_alerts();
        let (mut uploader, _) = uploader(&bucket, &dir, Duration::from_secs(3600));
        uploader.pass().await.unwrap();
        assert_eq!(bucket.with(|b| (b.objects.len(), b.created)), (0, 0), "{case}: nothing created");
        let alert = alerts.try_recv().expect("the damaged file is paged");
        assert_eq!(alert["alert"], "daily_snapshot_unreadable", "{case}");
        assert!(path.exists(), "{case}: it stays");
    }
    let (bucket, dir) = (StandIn::start().await, scratch_dir("snapshot-upload-gone"));
    let listing = publish_ticks(&dir, 3, 1_000);
    fs::remove_file(dir.join(&listing.file)).unwrap();
    let (mut uploader, metrics) = uploader(&bucket, &dir, Duration::from_secs(3600));
    uploader.pass().await.unwrap();
    assert_eq!(bucket.with(|b| (b.asked, b.created)), (0, 0), "a file the host no longer holds is not asked for");
    assert_eq!(metrics.snapshot_archive.pending.get(), 0);
}

/// A listing under a name its offset does not give, as a host could write over the manifest, absolute or climbing out
/// of the directory, names no file the uploader reads or removes and no key it asks for or writes, and pages
/// `daily_snapshot_unreadable` once.
#[tokio::test]
async fn a_listing_under_another_name_than_its_offsets_is_neither_read_nor_sent() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("snapshot-upload-names"));
    let outside = scratch_dir("snapshot-upload-names-outside");
    let listing = publish_ticks(&outside, 3, 1_000);
    let newest = publish_ticks(&dir, 9, 10 * DAY_MS);
    let mut absolute = listing.clone();
    absolute.file = outside.join(&listing.file).display().to_string();
    let mut climbing = listing.clone();
    climbing.file = format!("../{}/{}", outside.file_name().unwrap().to_string_lossy(), listing.file);
    crate::daily_snapshots::write_listing(&dir, &[absolute, climbing, newest]);
    let (_guard, mut alerts) = capture_alerts();
    let (mut uploader, metrics) = uploader(&bucket, &dir, Duration::from_secs(3600));
    uploader.pass().await.unwrap();
    uploader.pass().await.unwrap();
    assert_eq!(bucket.with(|b| (b.asked, b.created, b.objects.len())), (1, 1, 1), "only the canonical one");
    let paged: Vec<String> =
        std::iter::from_fn(|| alerts.try_recv().ok()).map(|alert| alert["alert"].clone()).collect();
    assert_eq!(paged, ["daily_snapshot_unreadable", "daily_snapshot_unreadable"], "each once");
    assert!(outside.join(&listing.file).exists(), "a file outside the directory is never removed");
    assert_eq!(metrics.snapshot_archive.removed.get(), 0);
}

/// A copy the window no longer keeps that cannot be removed stays confirmed and pages `daily_snapshot_unremovable`, and
/// goes once removal works, with no second read of its object, which resolves the alert.
#[cfg(unix)]
#[tokio::test]
async fn a_copy_that_cannot_be_removed_is_removed_later_without_another_read() {
    use std::os::unix::fs::PermissionsExt;
    let (bucket, dir) = (StandIn::start().await, scratch_dir("snapshot-upload-unremovable"));
    let days: Vec<Listed> = (0..3).map(|day| publish_ticks(&dir, 2 + day, day as i64 * DAY_MS * 2 + 1_000)).collect();
    let (mut uploader, metrics) = uploader(&bucket, &dir, Duration::from_secs(3600));
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let failed = (uploader.pass().await, uploader.pass().await);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(format!("{:#}", failed.0.unwrap_err()).contains("removing daily snapshot"));
    assert!(failed.1.is_err() && dir.join(&days[0].file).exists());
    let paged: Vec<String> =
        std::iter::from_fn(|| alerts.try_recv().ok()).map(|alert| alert["alert"].clone()).collect();
    assert_eq!(paged, ["daily_snapshot_unremovable"], "paged once");
    let reads = bucket.with(|b| b.read);
    uploader.pass().await.unwrap();
    assert!(!dir.join(&days[0].file).exists(), "removed once it can be");
    assert_eq!(bucket.with(|b| b.read), reads, "with no second read");
    assert_eq!(metrics.snapshot_archive.removed.get(), 1);
    assert_eq!(resolved.try_recv().expect("the alert resolves")["alert_resolved"], "daily_snapshot_unremovable");
}

/// The node's uploader comes from `[daily_snapshots]`, `[archive]` and both of its delivered keys, under the custody
/// chain and vault, and with any missing there is none.
#[test]
fn the_uploader_runs_only_with_both_sections_and_both_keys() {
    let mut cfg = crate::test_support::dev_config();
    let metrics = Arc::new(Metrics::new());
    let section = || -> crate::config::ArchiveSection {
        toml::from_str(
            "endpoint = 'https://account.r2.cloudflarestorage.com'\nbucket = 'engine-log'\nregion = 'auto'\n\
             public_url = 'https://archive.example.com'\naccess_key_id_secret = 'archive_access_key_id'\n\
             secret_access_key_secret = 'archive_secret_access_key'\n",
        )
        .unwrap()
    };
    let names = (section().access_key_id_secret, section().secret_access_key_secret);
    let value = crate::config::SecretValue::test;
    let one: Values = [(names.0.clone(), value("FAKE-id"))].into();
    let both: Values = [(names.0, value("FAKE-id")), (names.1, value("FAKE-secret"))].into();
    let daily = crate::config::DailySnapshotsSection::at(scratch_dir("snapshot-upload-config"));
    for (archive, daily, keys, runs) in [
        (Some(section()), None, both.clone(), false),
        (None, Some(daily.clone()), both.clone(), false),
        (Some(section()), Some(daily.clone()), one, false),
        (Some(section()), Some(daily), both, true),
    ] {
        (cfg.archive, cfg.daily_snapshots) = (archive, daily);
        let uploader = SnapshotUploader::for_config(&cfg, &keys, &metrics).unwrap();
        assert_eq!(uploader.is_some(), runs);
        let vault = cfg.custody.vault.parse().unwrap();
        let prefix = snapshots_prefix(cfg.custody.chain_id, vault);
        assert!(uploader.is_none_or(|uploader| uploader.prefix == prefix && uploader.window_ms == 30 * DAY_MS));
    }
}

/// A snapshot the bucket already holds, as another member uploaded it or this node did before a restart, is read
/// back with no upload, and confirmed.
#[tokio::test]
async fn a_snapshot_the_bucket_holds_is_read_back_with_no_upload() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("snapshot-upload-held"));
    let listing = publish_ticks(&dir, 3, 1_000);
    let written = fs::read(dir.join(&listing.file)).unwrap();
    bucket.with(|b| b.objects.insert(format!("{}{}", prefix(), listing.file), written));
    for _restart in 0..2 {
        let (mut uploader, metrics) = uploader(&bucket, &dir, Duration::from_secs(3600));
        uploader.pass().await.unwrap();
        assert_eq!(metrics.snapshot_archive.confirmed.get(), 1);
    }
    assert_eq!(bucket.with(|b| (b.created, b.refused, b.read)), (0, 0, 2), "read back at each start, never sent");
}

/// Every snapshot waiting for its confirmation counts from when it was first seen, those the pass has not reached
/// yet included; one that waits the lag alert's time pages `archive_upload_lagging`, keyed apart from the log's
/// ranges, while the host keeps it, and the alert resolves once the bucket takes them.
#[tokio::test]
async fn a_snapshot_waiting_past_the_lag_alert_is_paged_apart_from_the_logs_ranges() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("snapshot-upload-lagging"));
    publish_ticks(&dir, 3, 1_000);
    publish_ticks(&dir, 5, 2_000);
    let lag = Duration::from_secs(3600);
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let (mut uploader, metrics) = uploader(&bucket, &dir, lag);
    bucket.with(|b| b.down = true);
    uploader.pass().await.unwrap_err();
    assert_eq!(metrics.snapshot_archive.pending.get(), 2, "both wait, though the pass stopped at the newest");
    assert!(alerts.try_recv().is_err(), "not before the lag alert's time");
    uploader.check_at(Instant::now() + lag);
    let alert = alerts.try_recv().expect("the wait is paged");
    assert_eq!((alert["alert"].as_str(), alert["objects"].as_str()), ("archive_upload_lagging", "daily_snapshots"));
    bucket.with(|b| b.down = false);
    uploader.pass().await.unwrap();
    let resolution = resolved.try_recv().expect("the alert resolves");
    assert_eq!(resolution["alert_resolved"], "archive_upload_lagging");
    assert_eq!(resolution["objects"], "daily_snapshots");
    assert_eq!(metrics.snapshot_archive.pending.get(), 0);
}
