use super::*;
use crate::archive::stand_in::StandIn;

/// Lines shaped like a range's.
fn range_lines(commands: u64) -> Vec<u8> {
    (0..commands)
        .flat_map(|seq| {
            format!("{{\"entry\":\"command\",\"seq\":{seq},\"ts\":{},\"cmd\":\"tick\"}}\n", 1_700_000_000_000 + seq)
                .into_bytes()
        })
        .collect()
}

#[test]
fn a_compressed_range_holds_its_bytes_and_nothing_else() {
    let raw = range_lines(100);
    let stored = compress(&raw).unwrap();
    assert!(stored.len() < raw.len() / 4, "{} bytes from {}", stored.len(), raw.len());
    assert!(stored.len() as u64 <= stored_bound(&raw));
    assert!(holds(&stored, &raw), "the range's own frame");

    let mut altered = raw.clone();
    altered[10] ^= 1;
    assert!(!holds(&compress(&altered).unwrap(), &raw), "another range of the same length");
    assert!(!holds(&stored[..stored.len() - 1], &raw), "a cut frame");
    let longer = [raw.as_slice(), b"{}\n"].concat();
    assert!(!holds(&compress(&longer).unwrap(), &raw), "a frame that expands past the range");
    assert!(!holds(&compress(&raw[..raw.len() - 1]).unwrap(), &raw), "a frame one byte short");
    assert!(!holds(b"not a zstd frame", &raw), "bytes that are no frame");
    let other_level = zstd::bulk::compress(&raw, 19).unwrap();
    assert!(holds(&other_level, &raw), "another node's frame of the same bytes");
}

#[test]
fn a_frame_records_its_contents_checksum() {
    // Bit 2 of a frame header's descriptor, after the four magic bytes, is the content checksum flag.
    let stored = compress(&range_lines(10)).unwrap();
    assert_eq!(&stored[..4], &[0x28, 0xb5, 0x2f, 0xfd]);
    assert_eq!(stored[4] & 0b100, 0b100);
}

#[test]
fn each_range_is_keyed_by_its_first_seq_under_its_logs_chain_and_vault() {
    let vault = engine_core::types::AccountId([0xab; 20]);
    let log = log_prefix(42_161, vault);
    assert_eq!(log, format!("log/42161/0x{}/", "ab".repeat(20)));
    assert_eq!(range_key(&log, 0), format!("{log}00000000000000000000.jsonl.zst"));
    assert_eq!(range_key(&log, 3), format!("{log}{:020}.jsonl.zst", 3 * range_seqs()));
    assert_ne!(range_key(&log_prefix(42_161, engine_core::types::AccountId([0xac; 20])), 0), range_key(&log, 0));
}

/// The presigned URL AWS documents for Signature Version 4 query authentication, from the signer the
/// archive uploads with.
#[test]
fn the_signer_gives_the_presigned_url_aws_documents() {
    use rusty_s3::S3Action;
    use rusty_s3::actions::GetObject;
    let endpoint = "https://s3.amazonaws.com".parse().unwrap();
    let bucket =
        rusty_s3::Bucket::new(endpoint, rusty_s3::UrlStyle::VirtualHost, "examplebucket", "us-east-1").unwrap();
    let credentials = rusty_s3::Credentials::new("AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
    // Fri, 24 May 2013 00:00:00 GMT.
    let time = jiff::Timestamp::from_second(1_369_353_600).unwrap();
    let url = GetObject::new(&bucket, Some(&credentials), "test.txt")
        .sign_with_time(std::time::Duration::from_secs(86_400), &time);
    assert_eq!(
        url.as_str(),
        "https://examplebucket.s3.amazonaws.com/test.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256\
         &X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20130524T000000Z\
         &X-Amz-Expires=86400&X-Amz-SignedHeaders=host\
         &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
    );
}

#[tokio::test]
async fn an_object_is_created_once_and_never_replaced() {
    let bucket = StandIn::start().await;
    let store = bucket.store("log/");
    assert_eq!(store.create("log/a.zst", b"first".to_vec(), ZSTD).await.unwrap(), Created::New);
    assert_eq!(store.create("log/a.zst", b"second".to_vec(), ZSTD).await.unwrap(), Created::Existing);
    assert_eq!(bucket.with(|b| (b.created, b.refused, b.objects["log/a.zst"].clone())), (1, 1, b"first".to_vec()));
    assert_eq!(store.read("log/a.zst", 5).await.unwrap(), Object::Read(b"first".to_vec()));
    assert_eq!(store.read("log/a.zst", 4).await.unwrap(), Object::Oversized, "longer than the reader's bound");
    assert_eq!(store.read("log/b.zst", 5).await.unwrap(), Object::Absent);
    assert!(store.holds("log/a.zst").await.unwrap());
    assert!(!store.holds("log/b.zst").await.unwrap());
}

/// A key already taken is answered `412` by a bucket that checks the condition first, and `409` by one that meets a
/// concurrent upload; either way the object at the key is the one counted, and it stays.
#[tokio::test]
async fn a_conflicting_upload_counts_the_object_already_there() {
    let bucket = StandIn::start().await;
    let store = bucket.store("log/");
    assert_eq!(store.create("log/a.zst", b"first".to_vec(), ZSTD).await.unwrap(), Created::New);
    bucket.with(|b| b.conflict = true);
    assert_eq!(store.create("log/a.zst", b"second".to_vec(), ZSTD).await.unwrap(), Created::Existing);
    assert_eq!(bucket.with(|b| (b.refused, b.objects["log/a.zst"].clone())), (1, b"first".to_vec()));
}

#[tokio::test]
async fn an_upload_signed_with_another_key_is_refused() {
    let bucket = StandIn::start().await;
    let refused = bucket.store_with("log/", "another-secret").create("log/a.zst", b"first".to_vec(), ZSTD).await;
    let message = format!("{:#}", refused.unwrap_err());
    assert!(message.contains("403") && message.contains("(SignatureDoesNotMatch)"), "{message}");
    assert!(!message.contains("another-secret") && !message.contains("X-Amz"), "no credential is logged: {message}");
    assert_eq!(bucket.with(|b| (b.unsigned, b.objects.len())), (1, 0));
}

#[tokio::test]
async fn an_unavailable_bucket_is_a_failure_never_an_absence() {
    let bucket = StandIn::start().await;
    let store = bucket.store("log/");
    bucket.with(|b| b.down = true);
    assert!(store.create("log/a.zst", b"first".to_vec(), ZSTD).await.is_err());
    assert!(store.read("log/a.zst", 5).await.is_err());
    assert!(store.holds("log/a.zst").await.is_err());
    bucket.with(|b| b.down = false);
    assert_eq!(store.create("log/a.zst", b"first".to_vec(), ZSTD).await.unwrap(), Created::New);
}

/// An upload the bucket refuses names the refusal's code, read from the answer's first bytes alone however long
/// the answer runs.
#[tokio::test]
async fn a_refused_upload_names_its_code_from_the_answers_first_bytes() {
    let bucket = StandIn::start().await;
    bucket.with(|b| b.refuse_uploads = true);
    let refused = bucket.store("log/").create("log/a.zst", b"first".to_vec(), ZSTD).await;
    let message = format!("{:#}", refused.unwrap_err());
    assert!(message.contains("403") && message.contains("(AccessDenied)"), "{message}");
    assert!(message.len() < 300, "nothing of the answer's padding is kept: {} bytes", message.len());
}

/// A store reads and writes only plain names under its caller's key prefix: a key that climbs out of it, reaches a
/// segment below it, names another prefix or names nothing is refused before any request reaches the bucket.
#[tokio::test]
async fn a_key_that_is_no_plain_name_under_the_stores_prefix_is_refused_before_any_request() {
    let bucket = StandIn::start().await;
    let store = bucket.store("log/1/");
    let keys = [
        "log/1/../2/a.zst",
        "log/1/./a.zst",
        "log/1/sub/a.zst",
        "log/1/..",
        "log/1/.a",
        "log/1/",
        "log/1/a%2Fb.zst",
        "log/1/a?b.zst",
        "log/2/a.zst",
        "/log/1/a.zst",
        "snapshots/1/a.json",
    ];
    for key in keys {
        let created = store.create(key, b"first".to_vec(), ZSTD).await.map(|_| ());
        let read = store.read(key, 5).await.map(|_| ());
        let digested = store.digest(key, 5, std::time::Duration::from_secs(5)).await.map(|_| ());
        let held = store.holds(key).await.map(|_| ());
        for (what, refused) in [("created", created), ("read", read), ("digested", digested), ("asked for", held)] {
            let message = format!("{:#}", refused.expect_err(&format!("{key} is {what}")));
            assert!(message.contains("is not an object name under log/1/"), "{key} {what}: {message}");
        }
    }
    let requests = bucket.with(|b| (b.created, b.refused, b.unsigned, b.asked, b.read, b.objects.len()));
    assert_eq!(requests, (0, 0, 0, 0, 0, 0), "no request reaches the bucket");
    assert_eq!(store.create("log/1/a-0_b.zst", b"first".to_vec(), ZSTD).await.unwrap(), Created::New, "a plain name");
}

/// A store narrows only to a directory below its own prefix, each segment it adds a plain name, and a narrowed store, as
/// a checkpoint's tail's, reads and writes only plain names directly under that directory: neither the log's ranges
/// beside it, another tail's pieces, nor a key climbing out of it or reaching below it.
#[tokio::test]
async fn a_tails_store_writes_only_directly_under_its_checkpoints_directory() {
    let bucket = StandIn::start().await;
    let log = bucket.store("log/1/");
    let directories = [
        "log/1/",
        "log/1/tail",
        "log/1/tail//",
        "log/1/./",
        "log/1/../2/",
        "log/1/tail/../",
        "log/1/.tail/",
        "log/1/tail/a b/",
        "log/2/tail/",
        "tail/x/",
    ];
    for directory in directories {
        let message = format!("{:#}", log.within(directory).err().unwrap_or_else(|| panic!("{directory} is taken")));
        assert!(message.contains("is not a directory under log/1/"), "{directory}: {message}");
    }
    let root = engine_core::eip712::B256::repeat_byte(0xab);
    let tail = log.within(&tail_prefix(log.prefix(), 12, &root)).unwrap();
    assert_eq!(tail.prefix(), format!("log/1/tail/{:020}-0x{}/", 12, "ab".repeat(32)));
    let piece = format!("{}{}", tail.prefix(), range_key("", 2));
    let outside = [
        range_key(log.prefix(), 2),
        format!("{}{}", tail_prefix(log.prefix(), 18, &root), range_key("", 2)),
        format!("{}../{}", tail.prefix(), range_key("", 2)),
        format!("{}sub/{}", tail.prefix(), range_key("", 2)),
    ];
    for key in &outside {
        let created = tail.create(key, b"first".to_vec(), ZSTD).await.map(|_| ());
        let held = tail.holds(key).await.map(|_| ());
        for (what, refused) in [("created", created), ("asked for", held)] {
            let message = format!("{:#}", refused.expect_err(&format!("{key} is {what}")));
            assert!(message.contains("is not an object name under log/1/tail/"), "{key} {what}: {message}");
        }
    }
    let requests = bucket.with(|b| (b.created, b.asked, b.read, b.tail_created, b.tail_read, b.objects.len()));
    assert_eq!(requests, (0, 0, 0, 0, 0, 0), "no request reaches the bucket");
    assert_eq!(tail.create(&piece, b"first".to_vec(), ZSTD).await.unwrap(), Created::New, "a piece of the tail");
    assert_eq!(bucket.with(|b| b.objects.keys().cloned().collect::<Vec<_>>()), [piece]);
}
