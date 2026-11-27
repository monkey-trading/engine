use super::*;
use crate::archive::Bucket;
use crate::archive::read::origin::{copy_log, listed_below, log_of, serve, upload};
use crate::archive::read::tail::tests::{checkpoint_in, through, upload_tail};
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
    let parent = scratch_dir("source-url-downloads");
    std::fs::create_dir_all(&parent).unwrap();
    let log = PublishedLog::Url(node.url.parse().unwrap()).local_in(&parent, None).unwrap();
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
    let e = PublishedLog::Url(node.url.parse().unwrap()).local_in(&parent, None).unwrap_err();
    assert!(format!("{e:#}").contains(&last), "the failure names the segment: {e:#}");
    assert_eq!(downloads(&parent), 0, "a failed download leaves no directory");
}

/// An interrupted download stops and removes its directory, as a failed one does, though the archive never answers.
#[test]
fn an_interrupted_download_leaves_no_directory() {
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url: Url = format!("http://{}", silent.local_addr().unwrap()).parse().unwrap();
    let held = std::thread::spawn(move || silent.accept().map(|(stream, _)| stream));
    let parent = scratch_dir("source-interrupted");
    std::fs::create_dir_all(&parent).unwrap();
    let e = PublishedLog::Url(url).local_until(&parent, None, std::future::ready(())).unwrap_err();
    assert!(format!("{e:#}").contains("interrupted"), "{e:#}");
    assert_eq!(downloads(&parent), 0, "an interrupted download leaves no directory");
    drop(held);
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

/// `--log <a log's URL in a bucket>` reads the log from the bucket alone through the vault's checkpoint, over the
/// client the CLI uses, into a file in a directory of its own that goes with it; it needs the checkpoint, which no
/// other log takes.
#[test]
fn a_buckets_log_reads_through_the_vaults_checkpoint_into_a_file_removed_with_it() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (bucket, (dir, view)) = runtime.block_on(async { (StandIn::start().await, log_of("source-bucket", 5).await) });
    let (seq, root) = checkpoint_in(&dir, 3);
    upload(&bucket, &dir, &view, 2);
    upload_tail(&bucket, (&dir, &view), (seq, root), 2);
    let published: PublishedLog = format!("{}{}", bucket.plain, test_log()).parse().unwrap();
    assert!(matches!(published, PublishedLog::Bucket(_)), "{published:?}");
    let parent = scratch_dir("source-bucket-downloads");
    std::fs::create_dir_all(&parent).unwrap();
    let log = published.local_in(&parent, Some((seq, root))).unwrap();
    assert_eq!(std::fs::read(log.path()).unwrap(), through(&dir, &view, seq));
    drop(log);
    assert_eq!(downloads(&parent), 0, "the download goes with its log");

    let e = published.local_in(&parent, None).unwrap_err();
    assert!(format!("{e:#}").contains("their --seq and --state-root"), "{e:#}");
    for other in [PublishedLog::Path(dir.clone()), PublishedLog::Url("https://node.example".parse().unwrap())] {
        let e = other.local_in(&parent, Some((seq, root))).unwrap_err();
        assert!(format!("{e:#}").contains("holds its own"), "{e:#}");
    }
    assert_eq!(downloads(&parent), 0, "a refused read leaves no directory");
}

/// A URL whose path ends `log/<chain id>/<vault>` is a log in a bucket, read at its key prefix with the vault in lower
/// case whatever case it was given in, below any path the bucket's origin has; any other URL is a node's.
#[test]
fn a_logs_url_in_a_bucket_is_told_from_a_nodes() {
    let vault = "0xAbCdEf0123456789aBcDeF0123456789AbCdEf01";
    let lower = vault.to_lowercase();
    for (given, read) in [
        (format!("https://archive.example/log/42161/{vault}"), format!("https://archive.example/log/42161/{lower}/")),
        (format!("https://archive.example/log/42161/{lower}/"), format!("https://archive.example/log/42161/{lower}/")),
        (format!("https://cdn.example/engine/log/1/{lower}"), format!("https://cdn.example/engine/log/1/{lower}/")),
    ] {
        assert_eq!(given.parse::<PublishedLog>().unwrap(), PublishedLog::Bucket(read.parse().unwrap()), "{given}");
    }
    for node in [
        "https://node.example",
        "https://node.example/log/x/0x01",
        format!("https://node.example/logs/1/{lower}").as_str(),
    ] {
        assert!(matches!(node.parse::<PublishedLog>().unwrap(), PublishedLog::Url(_)), "{node}");
    }
}

// ── Helpers ──

/// How many downloads are under `parent`.
fn downloads(parent: &std::path::Path) -> usize {
    std::fs::read_dir(parent).unwrap().count()
}
