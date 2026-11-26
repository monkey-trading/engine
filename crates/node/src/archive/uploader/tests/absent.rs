//! Ranges whose segments are no longer on the host: the local window removes a segment only once the bucket holds
//! every range it holds seqs of, and the host may lose one before. The uploader puts such a segment back from the
//! bucket, as its listing names it, and reads the range back against it.

use super::*;
use crate::archive::absent_segments;
use crate::archive::read::origin::{copy_log, upload};

/// A resume whose last range in the bucket the host no longer holds a segment of puts that segment back from the
/// bucket and reads the range back against it, and the uploader goes on past it.
#[tokio::test]
async fn a_resume_takes_the_last_range_whose_segments_left_the_host_as_the_bucket_holds_it() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-absent-resume"));
    let node = node_with(&dir, 6).await;
    let view = node.manifest.borrow().clone();
    upload(&bucket, &dir, &view, 4);
    assert!(absent_segments(&dir, &view, 3).unwrap().is_empty(), "nothing absent before the window removes it");
    // As the window removes them: each segment whose every range the bucket holds.
    for listed in view.segments.iter().filter(|listed| listed.last_seq < 4 * RANGE_SEQS) {
        std::fs::remove_file(dir.join(&listed.file)).unwrap();
    }
    assert!(!absent_segments(&dir, &view, 3).unwrap().is_empty(), "the last range in the bucket lost a segment");
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), None);
    let archived = uploader.archived();
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let whole = whole_ranges(&view).unwrap();
    until("the ranges after it confirmed", || metrics.archive.confirmed_ranges.get() as u64 >= whole).await;
    assert_eq!(*archived.borrow(), Some(whole), "the window reads how many ranges the bucket holds");
    assert_eq!(metrics.archive.mismatches.get(), 0);
    assert!(absent_segments(&dir, &view, 3).unwrap().is_empty(), "the last range's segments are back on the host");
    running.abort();
    node.stop().await;
}

/// A range the bucket lacks, a segment of which the host lost, is not built: the uploader waits at it, the lag alert
/// firing, until another copy of the log puts it in the bucket, and then puts the segment back from there, reads the
/// range back against it, and goes on; while the bucket gives back other bytes, it waits still.
#[tokio::test]
async fn a_range_whose_segment_the_host_lost_waits_until_another_copy_uploads_it() {
    let (_guard, mut alerts) = capture_alerts();
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-absent-wait"));
    let node = node_with(&dir, 6).await;
    let view = node.manifest.borrow().clone();
    let copy = scratch_dir("archive-absent-copy");
    copy_log(&dir, &copy);
    upload(&bucket, &dir, &view, 2);
    let lost = view.segments.iter().find(|listed| listed.first_seq >= 2 * RANGE_SEQS).cloned().unwrap();
    std::fs::remove_file(dir.join(&lost.file)).unwrap();
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_millis(200), None);
    let archived = uploader.archived();
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let waiting = lost.first_seq / RANGE_SEQS;
    until("a failed attempt at the lost segment's range", || metrics.archive.failures.get() >= 2).await;
    assert_eq!(*archived.borrow(), Some(waiting), "the uploader waits at the range");
    recv(&mut alerts, "archive_upload_lagging").await;
    // A member's copy, which holds the segment, uploads the ranges, one of which the bucket first serves otherwise.
    let key = crate::archive::range_key(&crate::archive::stand_in::test_log(), waiting);
    bucket.with(|b| b.served.insert(key, b"other bytes".to_vec()));
    upload(&bucket, &copy, &view, whole_ranges(&view).unwrap());
    let failed = metrics.archive.failures.get();
    until("another failed attempt", || metrics.archive.failures.get() > failed + 1).await;
    assert_eq!(*archived.borrow(), Some(waiting), "nothing counts on bytes other than the listing's");
    bucket.with(|b| b.served.clear());
    let whole = whole_ranges(&view).unwrap();
    until("every range counted", || *archived.borrow() >= Some(whole)).await;
    assert_eq!(metrics.archive.mismatches.get(), 0);
    assert!(dir.join(&lost.file).exists(), "the lost segment is back on the host");
    running.abort();
    node.stop().await;
}
