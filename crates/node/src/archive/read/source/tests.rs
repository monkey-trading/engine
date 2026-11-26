use std::collections::BTreeSet;

use super::*;
use crate::archive::Bucket;
use crate::archive::read::origin::{copy_log, listed_below, log_of, serve, upload};
use crate::archive::stand_in::{StandIn, test_log};
use crate::test_support::{log_entries, scratch_dir};

/// `--log <URL>` downloads the archive at a node's URL over the client the CLI uses, the segments the node no longer
/// holds from the bucket it redirects to, into a directory of its own that holds the node's log and goes with it;
/// a download that fails partway leaves no directory behind.
#[test]
fn a_published_logs_url_downloads_into_a_directory_removed_with_it() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (bucket, (dir, view)) = runtime.block_on(async { (StandIn::start().await, log_of("source-url", 4).await) });
    upload(&bucket, &dir, &view, 4);
    let served = scratch_dir("source-url-served");
    copy_log(&dir, &served);
    for listed in listed_below(&dir, 4) {
        std::fs::remove_file(served.join(&listed.file)).unwrap();
    }
    let node = runtime.block_on(serve(&served, Some(Bucket::new(&bucket.plain, &test_log()).unwrap())));
    let log = PublishedLog::Url(node.url.parse().unwrap()).local().unwrap();
    assert_eq!(log_entries(log.path()), log_entries(&dir));
    assert!(bucket.with(|b| b.read) > 0, "the moved segments came from the bucket");
    let path = log.path().to_owned();
    drop(log);
    assert!(!path.exists(), "the download goes with its log");

    let cut = scratch_dir("source-url-cut");
    copy_log(&dir, &cut);
    let last = view.segments.last().unwrap().file.clone();
    std::fs::remove_file(cut.join(&last)).unwrap();
    let node = runtime.block_on(serve(&cut, None));
    let before = downloads();
    let e = PublishedLog::Url(node.url.parse().unwrap()).local().unwrap_err();
    assert!(format!("{e:#}").contains(&last), "the failure names the segment: {e:#}");
    assert_eq!(downloads(), before, "a failed download leaves no directory");
}

/// A path is a path, and an archive's URL is refused when it names a query, a fragment or credentials.
#[test]
fn an_archives_url_names_only_where_the_archive_is() {
    assert_eq!("data/log".parse::<PublishedLog>().unwrap(), PublishedLog::Path("data/log".into()));
    assert!(matches!("https://node.example".parse::<PublishedLog>().unwrap(), PublishedLog::Url(_)));
    for refused in ["https://node.example/?page=2", "https://node.example/#top", "https://reader:secret@node.example"] {
        let e = refused.parse::<PublishedLog>().unwrap_err();
        assert!(!format!("{e:#}").contains("secret"), "{e:#}");
    }
}

// ── Helpers ──

/// The downloads this process has under the system's temporary directory.
fn downloads() -> BTreeSet<std::path::PathBuf> {
    let prefix = format!("engine-archive-{}-", std::process::id());
    std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().starts_with(&prefix)))
        .collect()
}
