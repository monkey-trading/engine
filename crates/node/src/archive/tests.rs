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
    assert_eq!(range_key(&log, 3), format!("{log}{:020}.jsonl.zst", 3 * RANGE_SEQS));
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
    let store = bucket.store();
    assert_eq!(store.create("log/a.zst", b"first".to_vec(), ZSTD).await.unwrap(), Created::New);
    assert_eq!(store.create("log/a.zst", b"second".to_vec(), ZSTD).await.unwrap(), Created::Existing);
    assert_eq!(bucket.with(|b| (b.created, b.refused, b.objects["log/a.zst"].clone())), (1, 1, b"first".to_vec()));
    assert_eq!(store.read("log/a.zst", 5).await.unwrap(), Object::Read(b"first".to_vec()));
    assert_eq!(store.read("log/a.zst", 4).await.unwrap(), Object::Oversized, "longer than the reader's bound");
    assert_eq!(store.read("log/b.zst", 5).await.unwrap(), Object::Absent);
    assert!(store.holds("log/a.zst").await.unwrap());
    assert!(!store.holds("log/b.zst").await.unwrap());
}

#[tokio::test]
async fn an_upload_signed_with_another_key_is_refused() {
    let bucket = StandIn::start().await;
    let refused = bucket.store_with("another-secret").create("log/a.zst", b"first".to_vec(), ZSTD).await;
    let message = format!("{:#}", refused.unwrap_err());
    assert!(message.contains("403") && message.contains("(SignatureDoesNotMatch)"), "{message}");
    assert!(!message.contains("another-secret") && !message.contains("X-Amz"), "no credential is logged: {message}");
    assert_eq!(bucket.with(|b| (b.unsigned, b.objects.len())), (1, 0));
}

#[tokio::test]
async fn an_unavailable_bucket_is_a_failure_never_an_absence() {
    let bucket = StandIn::start().await;
    let store = bucket.store();
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
    let refused = bucket.store().create("log/a.zst", b"first".to_vec(), ZSTD).await;
    let message = format!("{:#}", refused.unwrap_err());
    assert!(message.contains("403") && message.contains("(AccessDenied)"), "{message}");
    assert!(message.len() < 300, "nothing of the answer's padding is kept: {} bytes", message.len());
}
