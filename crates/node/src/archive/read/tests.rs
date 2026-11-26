use super::*;
use crate::archive::read::origin::{bucket_of, listed_below, log_of, upload};
use crate::archive::stand_in::{StandIn, test_log};
use crate::archive::{compress, range_bytes};

/// Each segment below the bucket's ranges is the lines its ranges hold for its seqs, a segment that spans two
/// ranges included, and the reader keeps the object it read last for the seqs after: reading every segment in turn
/// reads each object once.
#[tokio::test]
async fn a_segment_is_rebuilt_from_the_ranges_that_hold_it() {
    let (bucket, (dir, view)) = (StandIn::start().await, log_of("archive-read-rebuilt", 4).await);
    upload(&bucket, &dir, &view, 4);
    let listed = listed_below(&dir, 4);
    assert!(listed.iter().any(|listed| listed.first_seq / RANGE_SEQS != listed.last_seq / RANGE_SEQS));
    let mut ranges = Ranges::new(bucket_of(&bucket));
    for listed in &listed {
        let rebuilt = ranges.seqs(&bucket.client, (listed.first_seq, listed.last_seq), listed.bytes).await.unwrap();
        assert_eq!(rebuilt, std::fs::read(dir.join(&listed.file)).unwrap(), "{}", listed.file);
    }
    let touched = listed.last().unwrap().last_seq / RANGE_SEQS + 1;
    assert_eq!(bucket.with(|b| b.read), touched, "each object read once");
}

/// A `404` for an object the seqs need names that range: never the end of what the bucket holds.
#[tokio::test]
async fn an_object_the_bucket_lacks_is_missing() {
    let (bucket, (dir, view)) = (StandIn::start().await, log_of("archive-read-missing", 3).await);
    upload(&bucket, &dir, &view, 3);
    bucket.with(|b| b.objects.remove(&range_key(&test_log(), 1)));
    let mut ranges = Ranges::new(bucket_of(&bucket));
    match ranges.seqs(&bucket.client, (2, 2 * RANGE_SEQS), u64::MAX).await {
        Err(Unread::Missing { index, object }) => {
            assert_eq!(index, 1);
            assert_eq!(object, bucket_of(&bucket).object(1).unwrap());
        }
        other => panic!("{other:?}"),
    }
}

/// An object that holds other seqs than its key's range, one cut short of the seqs needed, one that is no zstd frame,
/// and lines past the reader's bound are each refused, naming why.
#[tokio::test]
async fn an_object_that_holds_other_than_its_range_is_refused() {
    let (bucket, (dir, view)) = (StandIn::start().await, log_of("archive-read-refused", 3).await);
    let first = range_bytes(&dir, &view, 0).unwrap();
    let commands: Vec<&[u8]> = first.split_inclusive(|b| *b == b'\n').collect();
    let cut: Vec<u8> = commands.iter().take(2).flat_map(|line| line.iter().copied()).collect();
    // Each case: its name, the object, the seqs read, the reader's bound, and what the refusal names.
    let cases = [
        ("another range", compress(&first).unwrap(), (RANGE_SEQS, RANGE_SEQS), u64::MAX, "holds seq 0 where seq"),
        ("cut short", compress(&cut).unwrap(), (0, 3), u64::MAX, "before seq 3"),
        ("no frame", b"not zstd".to_vec(), (0, 0), u64::MAX, "zstd"),
        ("past the bound", compress(&first).unwrap(), (0, RANGE_SEQS - 1), 10, "pass 10 bytes"),
    ];
    for (case, object, seqs, limit, why) in cases {
        let index = seqs.0 / RANGE_SEQS;
        bucket.with(|b| b.objects.insert(range_key(&test_log(), index), object));
        match Ranges::new(bucket_of(&bucket)).seqs(&bucket.client, seqs, limit).await {
            Err(Unread::Refused(e)) => assert!(format!("{e:#}").contains(why), "{case}: {e:#}"),
            other => panic!("{case}: {other:?}"),
        }
    }
}

/// An object that decompresses past the reader's bound is refused once the bound is passed, however few of its
/// lines the seqs need.
#[tokio::test]
async fn an_object_that_decompresses_past_the_bound_is_refused() {
    let bucket = StandIn::start().await;
    let padding = "x".repeat(MAX_RANGE_BYTES as usize);
    let line = format!("{{\"entry\":\"command\",\"seq\":0,\"padding\":\"{padding}\"}}\n");
    bucket.with(|b| b.objects.insert(range_key(&test_log(), 0), compress(line.as_bytes()).unwrap()));
    match Ranges::new(bucket_of(&bucket)).seqs(&bucket.client, (0, 0), u64::MAX).await {
        Err(Unread::Refused(e)) => assert!(format!("{e:#}").contains("decompresses past"), "{e:#}"),
        other => panic!("{other:?}"),
    }
}

/// The reader reads each range at the URL the uploader reads it back at, and a node's redirect for a segment names
/// the object of the range holding its first seq, over `http` or `https`, from which the others are derived.
#[test]
fn a_reader_derives_each_object_from_the_bucket_and_a_redirect() {
    let public: Url = "https://archive.example".parse().unwrap();
    let prefix = log_prefix(42_161, engine_core::types::AccountId([0xab; 20]));
    let bucket = Bucket::new(&public, &prefix).unwrap();
    for index in [0, 1, 7] {
        assert_eq!(bucket.object(index).unwrap(), public.join(&range_key(&prefix, index)).unwrap());
    }
    let redirect = bucket.object(1).unwrap();
    let derived = Bucket::redirected(&redirect, RANGE_SEQS + 2).unwrap();
    assert_eq!(derived, bucket);
    assert_eq!(derived.object(2).unwrap(), bucket.object(2).unwrap());
    let refused = [
        (bucket.object(2).unwrap(), RANGE_SEQS + 2, "not the object of the range"),
        (public.join(&format!("{prefix}manifest.jsonl")).unwrap(), 0, "not the object of the range"),
        ("ftp://archive.example/log/0.jsonl.zst".parse().unwrap(), 0, "not over http"),
    ];
    for (location, first_seq, why) in refused {
        let e = Bucket::redirected(&location, first_seq).unwrap_err();
        assert!(format!("{e:#}").contains(why), "{location}: {e:#}");
    }
}
