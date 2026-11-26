use super::*;
use crate::archive::read::origin::{bucket_of, copy_log, listed_below, log_of, serve, upload};
use crate::archive::stand_in::{StandIn, test_log};
use crate::archive::{range_bytes, range_key};
use crate::standby::archive::{bootstrap_archive, fetch_manifest};
use crate::test_support::{capture_alerts, log_entries, scratch_dir};

/// A backfill takes the segments the bucket's ranges hold from the bucket, reading each object once, and asks the
/// node only for the segments past them, which the bucket has yet to hold: the copy is the node's log.
#[tokio::test]
async fn a_backfill_reads_the_bucket_first_and_the_node_past_it() {
    let (bucket, (dir, view)) = (StandIn::start().await, log_of("segments-first", 5).await);
    upload(&bucket, &dir, &view, 3);
    let node = serve(&dir, None).await;
    let copy = scratch_dir("segments-first-copy");
    let origins = Origins { bucket: Some(bucket_of(&bucket)), nodes: vec![node.url.clone()] };
    bootstrap_archive(&bucket.client, &origins, &copy).await.unwrap();
    assert_eq!(log_entries(&copy), log_entries(&dir));
    for listed in view.segments.iter() {
        let from_bucket = listed.last_seq < 3 * RANGE_SEQS;
        assert_eq!(node.asked(&listed.file), u64::from(!from_bucket), "{}", listed.file);
    }
    // The ranges it holds, each once, and the first it lacks, once: the nodes are asked first from there on.
    assert_eq!(bucket.with(|b| b.read), 3 + 1);
}

/// An object whose bytes are not those its segments' listings name is refused, and those segments come from the
/// node; the bucket is still read first for the segments after it.
#[tokio::test]
async fn a_segment_the_bucket_holds_otherwise_comes_from_the_node() {
    let (bucket, (dir, view)) = (StandIn::start().await, log_of("segments-otherwise", 4).await);
    upload(&bucket, &dir, &view, 4);
    let altered = range_bytes(&dir, &view, 1).unwrap().to_ascii_uppercase();
    bucket.with(|b| b.served.insert(range_key(&test_log(), 1), crate::archive::compress(&altered).unwrap()));
    let node = serve(&dir, None).await;
    let copy = scratch_dir("segments-otherwise-copy");
    let origins = Origins { bucket: Some(bucket_of(&bucket)), nodes: vec![node.url.clone()] };
    bootstrap_archive(&bucket.client, &origins, &copy).await.unwrap();
    assert_eq!(log_entries(&copy), log_entries(&dir));
    let in_range = |listed: &SegmentInfo, index: u64| {
        (listed.first_seq / RANGE_SEQS..=listed.last_seq / RANGE_SEQS).contains(&index)
    };
    for listed in listed_below(&dir, 4) {
        assert_eq!(node.asked(&listed.file), u64::from(in_range(&listed, 1)), "{}", listed.file);
    }
}

/// A node that no longer holds a segment redirects to the bucket, which a reader with no bucket of its own then
/// reads, deriving the keys of the segment's other ranges, and reads first for the segments after it.
#[tokio::test]
async fn a_node_that_no_longer_holds_a_segment_redirects_its_reader_to_the_bucket() {
    let (bucket, (dir, view)) = (StandIn::start().await, log_of("segments-redirected", 5).await);
    upload(&bucket, &dir, &view, 4);
    let served = scratch_dir("segments-redirected-served");
    copy_log(&dir, &served);
    let moved = listed_below(&dir, 4);
    for listed in &moved {
        std::fs::remove_file(served.join(&listed.file)).unwrap();
    }
    let node = serve(&served, Some(bucket_of(&bucket))).await;
    let copy = scratch_dir("segments-redirected-copy");
    bootstrap_archive(&bucket.client, std::slice::from_ref(&node.url), &copy).await.unwrap();
    assert_eq!(log_entries(&copy), log_entries(&dir));
    assert_eq!(node.asked(&moved[0].file), 1, "the first moved segment redirects the reader");
    assert!(moved[1..].iter().all(|listed| node.asked(&listed.file) == 0), "the bucket is read first after it");
}

/// A reader whose own `[archive]` names a bucket follows no redirect to another: a node origin cannot send it to
/// read elsewhere, and the segment fails.
#[tokio::test]
async fn a_reader_with_its_own_bucket_follows_no_redirect_to_another() {
    let (bucket, (dir, view)) = (StandIn::start().await, log_of("segments-foreign", 2).await);
    upload(&bucket, &dir, &view, 2);
    let served = scratch_dir("segments-foreign-served");
    copy_log(&dir, &served);
    let first = listed_below(&dir, 2).remove(0);
    std::fs::remove_file(served.join(&first.file)).unwrap();
    let node = serve(&served, Some(bucket_of(&bucket))).await;
    let own = Bucket::new(&bucket.url, "log/1/0x0000000000000000000000000000000000000001/").unwrap();
    let origins = Origins { bucket: Some(own), nodes: vec![node.url.clone()] };
    let e = Segments::new(&bucket.client, &origins).segment(&first).await.unwrap_err();
    assert!(format!("{e:#}").contains("leaves the bucket [archive] names"), "{e:#}");
}

/// The bucket holds a prefix of the log, so past its first `404` the nodes are asked first; a segment none of them
/// serves is read from the bucket once more, as a cache in front of it may have answered a stale `404`.
#[tokio::test]
async fn a_segment_no_node_serves_is_asked_of_the_bucket_again_past_a_404() {
    let (bucket, (dir, view)) = (StandIn::start().await, log_of("segments-stale", 4).await);
    upload(&bucket, &dir, &view, 4);
    bucket.with(|b| b.objects.remove(&range_key(&test_log(), 1)));
    let served = scratch_dir("segments-stale-served");
    copy_log(&dir, &served);
    let late: Vec<SegmentInfo> =
        listed_below(&dir, 4).into_iter().filter(|listed| listed.first_seq >= 2 * RANGE_SEQS).collect();
    for listed in &late {
        std::fs::remove_file(served.join(&listed.file)).unwrap();
    }
    let node = serve(&served, None).await;
    let origins = Origins { bucket: Some(bucket_of(&bucket)), nodes: vec![node.url.clone()] };
    let mut segments = Segments::new(&bucket.client, &origins);
    let listings = segments.manifest().await.unwrap();
    for listed in listings.iter().filter(|listed| listed.last_seq < 4 * RANGE_SEQS) {
        let bytes = segments.segment(listed).await.unwrap();
        assert_eq!(bytes, std::fs::read(dir.join(&listed.file)).unwrap(), "{}", listed.file);
    }
    assert!(late.iter().all(|listed| node.asked(&listed.file) == 1), "the node is asked first past the 404");
}

/// A segment the bucket answers `404` for and no node serves fails the backfill, never ends it, and pages Warning
/// `archive_segment_missing` once for the object, however many of its segments the reader asks for.
#[tokio::test]
async fn a_segment_no_source_holds_is_missing_and_pages_once() {
    let (_guard, mut alerts) = capture_alerts();
    let (bucket, (dir, view)) = (StandIn::start().await, log_of("segments-missing", 4).await);
    upload(&bucket, &dir, &view, 4);
    bucket.with(|b| b.objects.remove(&range_key(&test_log(), 2)));
    let served = scratch_dir("segments-missing-served");
    copy_log(&dir, &served);
    let gone: Vec<SegmentInfo> = listed_below(&dir, 4)
        .into_iter()
        .filter(|listed| (listed.first_seq / RANGE_SEQS..=listed.last_seq / RANGE_SEQS).contains(&2))
        .collect();
    for listed in &gone {
        std::fs::remove_file(served.join(&listed.file)).unwrap();
    }
    let node = serve(&served, None).await;
    let origins = Origins { bucket: Some(bucket_of(&bucket)), nodes: vec![node.url.clone()] };
    let mut segments = Segments::new(&bucket.client, &origins);
    for listed in &gone {
        let e = segments.segment(listed).await.unwrap_err();
        assert!(format!("{e:#}").contains("archive_segment_missing"), "{e:#}");
        assert!(format!("{e:#}").contains(&listed.file), "{e:#}");
    }
    let object = bucket_of(&bucket).object(2).unwrap().to_string();
    let paged: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok())
        .filter(|line| line.get("alert").is_some_and(|name| name == "archive_segment_missing"))
        .collect();
    assert_eq!(paged.len(), 1, "{paged:?}");
    assert_eq!(paged[0]["object"], object);
    assert_eq!(paged[0]["level"], "WARN");
    let copy = scratch_dir("segments-missing-copy");
    let e = bootstrap_archive(&bucket.client, &origins, &copy).await.unwrap_err();
    assert!(format!("{e:#}").contains("archive_segment_missing"), "the backfill stops at the object: {e:#}");
}

/// A segment the bucket rebuilds to other bytes than its listing is no integrity failure of the archive, since the
/// uploader takes a range spelt otherwise as the log's, so a handover's sync does not refuse its start for it; one a
/// node serves otherwise is.
#[tokio::test]
async fn only_a_node_serving_other_bytes_fails_the_archives_integrity() {
    let (bucket, (dir, view)) = (StandIn::start().await, log_of("segments-integrity", 2).await);
    upload(&bucket, &dir, &view, 2);
    let altered = range_bytes(&dir, &view, 0).unwrap();
    let altered = String::from_utf8(altered).unwrap().replacen("\"ts\":1", "\"ts\":2", 1);
    bucket.with(|b| b.served.insert(range_key(&test_log(), 0), crate::archive::compress(altered.as_bytes()).unwrap()));
    let first = listed_below(&dir, 2).remove(0);
    let origins = Origins { bucket: Some(bucket_of(&bucket)), nodes: Vec::new() };
    let e = Segments::new(&bucket.client, &origins).segment(&first).await.unwrap_err();
    assert!(format!("{e:#}").contains("differs from its listing"), "{e:#}");
    assert!(e.downcast_ref::<crate::standby::archive::ArchiveIntegrity>().is_none(), "{e:#}");

    let served = scratch_dir("segments-integrity-served");
    copy_log(&dir, &served);
    std::fs::write(served.join(&first.file), altered.as_bytes()).unwrap();
    let node = serve(&served, None).await;
    let e = Segments::new(&bucket.client, &[node.url]).segment(&first).await.unwrap_err();
    assert!(e.downcast_ref::<crate::standby::archive::ArchiveIntegrity>().is_some(), "{e:#}");
}

/// A listing that names more bytes than a node serves a segment is rebuilt no further than that bound, however many
/// of the bucket's ranges its seqs span.
#[tokio::test]
async fn a_rebuild_reads_no_further_than_a_segments_bound() {
    let bucket = StandIn::start().await;
    let padding = "x".repeat(1024);
    let ranges = 200u64;
    for index in 0..ranges {
        let lines: String = (index * RANGE_SEQS..(index + 1) * RANGE_SEQS)
            .map(|seq| format!("{{\"entry\":\"command\",\"seq\":{seq},\"padding\":\"{padding}\"}}\n"))
            .collect();
        bucket.with(|b| {
            b.objects.insert(range_key(&test_log(), index), crate::archive::compress(lines.as_bytes()).unwrap())
        });
    }
    let listed = SegmentInfo {
        file: crate::log::segment_name(0),
        first_seq: 0,
        last_seq: ranges * RANGE_SEQS - 1,
        bytes: u64::MAX,
        sha256: "ab".repeat(32),
    };
    let origins = Origins { bucket: Some(bucket_of(&bucket)), nodes: Vec::new() };
    let e = Segments::new(&bucket.client, &origins).segment(&listed).await.unwrap_err();
    assert!(format!("{e:#}").contains(&format!("in at most {MAX_REBUILT_BYTES} bytes")), "{e:#}");
    assert!(bucket.with(|b| b.read) < ranges, "the rebuild stopped at the bound");
}

/// A bucket that takes the connection and never answers is unreachable within the wait a node fetch has, and the
/// segments come from the node, asked first from then on.
#[tokio::test]
async fn a_bucket_that_never_answers_gives_way_to_the_node() {
    let (dir, _) = log_of("segments-silent", 2).await;
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let public: Url = format!("http://{}/", silent.local_addr().unwrap()).parse().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = silent.accept().await {
            held.push(socket);
        }
    });
    let node = serve(&dir, None).await;
    let origins = Origins { bucket: Some(Bucket::new(&public, &test_log()).unwrap()), nodes: vec![node.url.clone()] };
    let (copy, started) = (scratch_dir("segments-silent-copy"), std::time::Instant::now());
    bootstrap_archive(&reqwest::Client::new(), &origins, &copy).await.unwrap();
    assert_eq!(log_entries(&copy), log_entries(&dir));
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "waited {:?} on the bucket", started.elapsed());
}

/// A manifest listing more segments than any fixed count a log reaches reads whole.
#[tokio::test]
async fn a_backfill_reads_a_manifest_of_more_than_65536_listings() {
    let dir = scratch_dir("segments-long-manifest");
    std::fs::create_dir_all(&dir).unwrap();
    let listings: String = (0..70_000u64)
        .map(|n| {
            let listed = SegmentInfo {
                file: crate::log::segment_name(n * 10),
                first_seq: n * 10,
                last_seq: n * 10 + 9,
                bytes: 5_500_000,
                sha256: "ab".repeat(32),
            };
            format!("{}\n", serde_json::to_string(&listed).unwrap())
        })
        .collect();
    std::fs::write(dir.join(crate::log::MANIFEST), listings).unwrap();
    let node = serve(&dir, None).await;
    let read = fetch_manifest(&crate::standby::archive::archive_client(false).unwrap(), &[node.url]).await.unwrap();
    assert_eq!(read.len(), 70_000);
    assert_eq!(read.last().unwrap().first_seq, 699_990);
}
