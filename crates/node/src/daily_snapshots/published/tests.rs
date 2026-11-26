//! `engine replay --snapshots` from a node's URL: the snapshots its manifest lists, from its disk or the bucket.

use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;

use super::*;
use crate::api::ServedSnapshots;
use crate::archive::stand_in::StandIn;
use crate::daily_snapshots::{Listed, MANIFEST, SnapshotBucket, publish_ticks, snapshots_prefix};
use crate::test_support::scratch_dir;

/// `--snapshots` takes a node's URL as `--log` does, and a path otherwise.
#[test]
fn snapshots_are_a_path_or_a_nodes_url() {
    assert_eq!("daily".parse::<PublishedSnapshots>().unwrap(), PublishedSnapshots::Path("daily".into()));
    let url = "https://node.example".parse::<PublishedSnapshots>().unwrap();
    assert_eq!(url, PublishedSnapshots::Url("https://node.example".parse().unwrap()));
    assert!("https://node.example/?key=1".parse::<PublishedSnapshots>().is_err(), "no query");
}

/// A node's listed snapshots at or before the log's last checkpoint download under their names, those it holds from
/// its disk and one it no longer holds from the bucket its `308` names, each as its listing names it; one past the
/// checkpoint, or listed under another name, is not fetched, and nothing is fetched for a log with no checkpoint.
#[tokio::test]
async fn a_nodes_snapshots_download_from_its_disk_and_the_bucket() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("published-snapshots-node"));
    let listed = [publish_ticks(&dir, 3, 1_000), publish_ticks(&dir, 5, 2_000), publish_ticks(&dir, 9, 3_000)];
    let moved =
        SnapshotBucket::new(&bucket.url, &snapshots_prefix(31_337, engine_core::types::AccountId([0; 20]))).unwrap();
    archive(&bucket, &moved, &dir, &listed[0]);
    fs::remove_file(dir.join(&listed[0].file)).unwrap();
    let mut misnamed = listed[1].clone();
    misnamed.file = "../escape.json".into();
    let manifest: String = listed.iter().chain([&misnamed]).map(|l| serde_json::to_string(l).unwrap() + "\n").collect();
    fs::write(dir.join(MANIFEST), manifest).unwrap();
    let url: Url = serve(&dir, Some(moved.clone())).await.parse().unwrap();

    let local = scratch_dir("published-snapshots-local");
    assert_eq!(download_listed(&bucket.client, &url, &local, Some(5)).await.unwrap(), 2);
    let written = |listing: &Listed| fs::read(local.join(&listing.file)).unwrap();
    assert_eq!(written(&listed[0]), bucket.with(|b| b.objects.values().next().cloned()).unwrap(), "from the bucket");
    assert_eq!(written(&listed[1]), fs::read(dir.join(&listed[1].file)).unwrap(), "from the node's disk");
    assert!(!local.join(&listed[2].file).exists(), "past the log's last checkpoint");
    let none = scratch_dir("published-snapshots-none");
    assert_eq!(download_listed(&bucket.client, &url, &none, None).await.unwrap(), 0);
}

/// A bucket object whose bytes are altered is refused, naming the snapshot, and the replay does not start.
#[tokio::test]
async fn an_altered_bucket_object_is_refused() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("published-snapshots-altered"));
    let listing = publish_ticks(&dir, 3, 1_000);
    let moved = SnapshotBucket::new(&bucket.url, "snapshots/1/0x02/").unwrap();
    archive(&bucket, &moved, &dir, &listing);
    let key = object_key(&moved, &listing);
    bucket.with(|b| {
        let mut altered = b.objects[&key].clone();
        altered[30] ^= 1;
        b.served.insert(key.clone(), altered)
    });
    fs::remove_file(dir.join(&listing.file)).unwrap();
    let url: Url = serve(&dir, Some(moved)).await.parse().unwrap();
    let error = download_listed(&bucket.client, &url, &scratch_dir("published-snapshots-refused"), Some(u64::MAX))
        .await
        .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains(&listing.file) && error.contains("differs from its listing"), "{error}");
}

// ── Helpers ──

/// The key of `listing`'s object under `bucket`.
fn object_key(bucket: &SnapshotBucket, listing: &Listed) -> String {
    bucket.object(&listing.file).unwrap().path().trim_start_matches('/').to_owned()
}

/// Puts `listing`'s snapshot in `dir` into the stand-in `bucket` under `moved`'s key, as the uploader does.
pub(crate) fn archive(bucket: &StandIn, moved: &SnapshotBucket, dir: &Path, listing: &Listed) {
    let written = fs::read(dir.join(&listing.file)).unwrap();
    bucket.with(|b| b.objects.insert(object_key(moved, listing), written));
}

/// Serves the daily snapshots in `dir` from a node's archive route, those its host no longer holds redirected to
/// `moved`, over plain HTTP on loopback: its base URL.
pub(crate) async fn serve(dir: &Path, moved: Option<SnapshotBucket>) -> String {
    let (mut state, _requests) = crate::api::tests::api_state(true);
    state.daily_snapshots = Some(Arc::new(ServedSnapshots::new(dir.to_owned(), moved)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = crate::api::router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}")
}
