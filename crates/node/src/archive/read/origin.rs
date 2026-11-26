//! Test stand-ins for what a reader of the archive reads: a log written to whole ranges in the bucket's stand-in,
//! and a node origin serving a log directory's archive, which redirects a listed segment it no longer holds to the
//! bucket as the node's archive route does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::StatusCode;
use axum::http::header::LOCATION;
use axum::response::IntoResponse;
use axum::routing::get;
use engine_core::command::Command;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::archive::stand_in::{StandIn, test_log};
use crate::archive::{Bucket, RANGE_SEQS, compress, range_bytes, range_key};
use crate::log::{MANIFEST, Manifest, SegmentInfo, SegmentedLog};
use crate::test_support::{SEGMENT_COMMANDS, TestSequencer, published_manifest, scratch_dir};

/// A node origin serving a log directory's archive, counting the requests for each file.
pub(crate) struct Origin {
    pub url: String,
    requests: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl Origin {
    /// How many times `file` was asked for.
    pub(crate) fn asked(&self, file: &str) -> u64 {
        self.requests.lock().expect("the origin's counts").get(file).copied().unwrap_or(0)
    }
}

/// A test sequencer's log in a new directory named for `name`, written until its completed segments hold `ranges`
/// whole ranges, its last segment then sealed, as the archive serves only completed ones, with its listing.
pub(crate) async fn log_of(name: &str, ranges: u64) -> (PathBuf, Manifest) {
    let dir = scratch_dir(name);
    let node = TestSequencer::start(&dir, 256);
    while node.manifest.borrow().unlisted_from().expect("the writer's listing") < ranges * RANGE_SEQS {
        node.submit(Command::Tick).await;
    }
    node.stop().await;
    let log = SegmentedLog::open(&dir, |_| Ok(())).expect("the log opens");
    let mut writer = log.into_writer(SEGMENT_COMMANDS, watch::channel(Manifest::default()).0).expect("a writer");
    writer.seal_open_segment().expect("the last segment sealed");
    drop(writer);
    let view = published_manifest(&dir).borrow().clone();
    (dir, view)
}

/// Puts the first `ranges` ranges of the log in `dir` into `bucket`'s stand-in, compressed, as the uploader does.
pub(crate) fn upload(bucket: &StandIn, dir: &Path, view: &Manifest, ranges: u64) {
    for index in 0..ranges {
        let object = compress(&range_bytes(dir, view, index).expect("a whole range")).expect("compressed");
        bucket.with(|b| b.objects.insert(range_key(&test_log(), index), object));
    }
}

/// The bucket's stand-in as a reader names it.
pub(crate) fn bucket_of(bucket: &StandIn) -> Bucket {
    Bucket::new(&bucket.url, &test_log()).expect("the stand-in's bucket")
}

/// The listings of the log in `dir` whose seqs all fall in its first `ranges` ranges.
pub(crate) fn listed_below(dir: &Path, ranges: u64) -> Vec<SegmentInfo> {
    let view = published_manifest(dir).borrow().clone();
    view.segments.iter().filter(|listed| listed.last_seq < ranges * RANGE_SEQS).cloned().collect()
}

/// Serves the archive of the log in `dir`: its manifest and each file it holds. A segment `dir`'s manifest lists
/// that is not there answers `308` to `moved`'s object of the range holding its first seq, or `404` without it.
pub(crate) async fn serve(dir: &Path, moved: Option<Bucket>) -> Origin {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let url = format!("http://{}", listener.local_addr().expect("local addr"));
    let (dir, requests) = (dir.to_owned(), Arc::new(Mutex::new(BTreeMap::<String, u64>::new())));
    let counts = requests.clone();
    let app = Router::new().route(
        "/v1/archive/{file}",
        get(move |axum::extract::Path(file): axum::extract::Path<String>| {
            *counts.lock().expect("the origin's counts").entry(file.clone()).or_default() += 1;
            let (path, listings, moved) = (dir.join(&file), dir.join(MANIFEST), moved.clone());
            async move {
                if let Ok(bytes) = std::fs::read(&path) {
                    return bytes.into_response();
                }
                let listed = std::fs::read(listings).unwrap_or_default().split(|b| *b == b'\n').find_map(|line| {
                    serde_json::from_slice::<SegmentInfo>(line).ok().filter(|listed| listed.file == file)
                });
                match (listed, moved) {
                    (Some(listed), Some(bucket)) => {
                        let object = bucket.object(listed.first_seq / RANGE_SEQS).expect("an object URL");
                        (StatusCode::PERMANENT_REDIRECT, [(LOCATION, object.to_string())]).into_response()
                    }
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.expect("the origin serves") });
    Origin { url, requests }
}

/// Copies the log directory `from`, its segments and its manifest, to `to`.
pub(crate) fn copy_log(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("the copy's directory");
    for file in std::fs::read_dir(from).expect("the log's directory") {
        let path = file.expect("a log file").path();
        let name = path.file_name().expect("a file name").to_string_lossy().into_owned();
        if crate::log::segment_seq(&name).is_some() || name == MANIFEST {
            std::fs::copy(&path, to.join(name)).expect("a copied file");
        }
    }
}
