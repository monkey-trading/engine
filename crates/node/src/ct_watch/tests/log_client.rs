use super::*;

use std::time::Duration;

use reqwest::Url;

use crate::ct_watch::log_client::{LogClient, LogKey, MAX_BATCH};

fn client_for(log: &StubLog, key: &TestLogKey) -> LogClient {
    let http = crate::benchmarks::client().unwrap();
    LogClient::new(http, Url::parse(&log.url).unwrap(), LogKey::parse(&key.spki_b64()).unwrap())
}

#[tokio::test]
async fn a_signed_tree_head_verifies_and_reports_its_size() {
    let log = StubLog::start(vec![vec![1], vec![2], vec![3]]).await;
    let head = client_for(&log, &log.key).tree_head().await.unwrap();
    assert_eq!((head.tree_size, head.timestamp_ms), (3, 1_700_000_000_000));
}

#[tokio::test]
async fn a_tree_head_signed_by_another_key_is_refused() {
    let log = StubLog::start(vec![vec![1]]).await;
    log.set_fault(Fault::WrongKey);
    let err = client_for(&log, &log.key).tree_head().await.unwrap_err();
    assert!(format!("{err:#}").contains("does not verify"), "{err:#}");
}

/// Serves `head` as the only tree head of a log and asks its client for it.
async fn tree_head_of(head: Value, key: &TestLogKey) -> anyhow::Result<crate::ct_watch::log_client::TreeHead> {
    use axum::Json;
    let app = Router::new().route("/ct/v1/get-sth", get(move || async move { Json(head.clone()) }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = crate::benchmarks::client().unwrap();
    let client = LogClient::new(http, Url::parse(&url).unwrap(), LogKey::parse(&key.spki_b64()).unwrap());
    let head = client.tree_head().await;
    server.abort();
    head
}

#[tokio::test]
async fn a_tree_head_altered_after_signing_is_refused() {
    let key = TestLogKey::new();
    let signed = key.tree_head(1_700_000_000_000, 10);
    assert_eq!(tree_head_of(signed.clone(), &key).await.unwrap().tree_size, 10);

    let mut bigger = signed.clone();
    bigger["tree_size"] = json!(11);
    assert!(tree_head_of(bigger, &key).await.is_err());
    let mut later = signed.clone();
    later["timestamp"] = json!(1_700_000_000_001u64);
    assert!(tree_head_of(later, &key).await.is_err());
    let mut other_root = signed.clone();
    other_root["sha256_root_hash"] = json!(STANDARD.encode([8u8; 32]));
    assert!(tree_head_of(other_root, &key).await.is_err());

    // The same signature under a header naming another hash or signature algorithm, or a length
    // that does not match, is not the signed structure.
    let signature = STANDARD.decode(signed["tree_head_signature"].as_str().unwrap()).unwrap();
    for (index, value) in [(0, 5u8), (1, 1), (3, signature[3].wrapping_add(1))] {
        let mut forged = signature.clone();
        forged[index] = value;
        let mut head = signed.clone();
        head["tree_head_signature"] = json!(STANDARD.encode(forged));
        assert!(tree_head_of(head, &key).await.is_err(), "header byte {index} = {value}");
    }
}

#[tokio::test]
async fn entries_come_back_in_order_as_a_prefix_of_the_range() {
    let leaves: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i]).collect();
    let log = StubLog::start(leaves.clone()).await;
    let client = client_for(&log, &log.key);
    assert_eq!(client.leaves(1, 4).await.unwrap(), leaves[1..3], "the log answers two a call");
    assert_eq!(client.leaves(4, 4).await.unwrap(), leaves[4..5]);
    assert!(log.shared.lock().unwrap().requests.contains(&"get-entries 1-4".to_owned()));
}

#[tokio::test]
async fn bad_log_answers_are_refused() {
    let log = StubLog::start(vec![vec![1], vec![2], vec![3]]).await;
    let client = client_for(&log, &log.key);
    for (fault, needle) in [
        (Fault::TooManyEntries, "more entries than asked"),
        (Fault::NoEntries, "no entries"),
        (Fault::NotBase64, "not base64"),
        (Fault::NotJson, "expected JSON"),
        (Fault::HugeBody, "exceeds"),
        (Fault::ServerError, "502"),
    ] {
        log.set_fault(fault);
        let err = client.leaves(0, 2).await.unwrap_err();
        assert!(format!("{err:#}").contains(needle), "{fault:?}: {err:#}");
    }
    log.set_fault(Fault::ServerError);
    assert!(format!("{:#}", client.tree_head().await.unwrap_err()).contains("500"));
}

#[tokio::test]
async fn a_range_that_is_empty_or_longer_than_a_batch_is_never_requested() {
    let log = StubLog::start(vec![vec![1]]).await;
    let client = client_for(&log, &log.key);
    assert!(client.leaves(5, 4).await.is_err());
    assert!(client.leaves(0, MAX_BATCH).await.is_err());
    assert!(log.shared.lock().unwrap().requests.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_log_that_never_answers_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    // Accepts connections and never replies.
    let silent = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok(connection) = listener.accept().await {
            held.push(connection);
        }
    });
    let key = TestLogKey::new();
    let http = crate::benchmarks::client().unwrap();
    let client = LogClient::new(http, Url::parse(&url).unwrap(), LogKey::parse(&key.spki_b64()).unwrap());
    let outcome = tokio::spawn(async move { client.tree_head().await });
    tokio::time::sleep(Duration::from_secs(30)).await;
    let err = outcome.await.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("timed out"), "{err:#}");
    silent.abort();
}

/// A P-384 public key, DER SubjectPublicKeyInfo in base64.
const P384_SPKI: &str = "MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAEWT40oGr/KqNV85gp0XXwe25eXs5Mq9/KRFnSvwpHLKArnlEsvf+b/pNz5om4aGxOgz9tWvGEzhbQ+ZCADbdwDBj3SV/VwmAs6wNmQRqYsRDLfpq/3kvQHndS2pMSg91O";
/// A 2048-bit RSA public key, DER SubjectPublicKeyInfo in base64.
const RSA_SPKI: &str = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA63a1+30ADmFrHkgU88542RU+7RHKY/1u9IpjLLcqKuXWIKHFeXfpLDNg1t4+J7ddJwp40k+Rm3YD6YFVac45XnWhfAAiHJDnPN/7WPOsD0Yv3myjP5DNSbF8Iklo5tcz2eVC3WN7urH/RdZgM1bNBDARcpG/7Fgiul8wF9pHFMrjFmFLtvsaRafl09JREpoMrQO5bPU693zTg1f3LO9RIFg7pzxim535q6agAhRQdM4BbHPoDIsybZ3Zq35tdGu9dVwMCQchTReDXh7wwNqIeA5/cGYXMhfv82m0OmH7sL3R5UGzbXkInUE4/lzpS7YD7Q7xBwhEMMI/n3xt8b6dvwIDAQAB";

#[test]
fn only_p256_and_rsa_keys_parse() {
    let key = TestLogKey::new();
    assert!(LogKey::parse(&key.spki_b64()).is_ok());
    assert!(LogKey::parse(RSA_SPKI).is_ok());
    for (bad, why) in [("!!", "not base64"), ("AAAA", "not DER"), (P384_SPKI, "another curve")] {
        assert!(LogKey::parse(bad).is_err(), "{why}");
    }
    // Trailing bytes after a valid key.
    let mut der = STANDARD.decode(key.spki_b64()).unwrap();
    der.push(0);
    assert!(LogKey::parse(&STANDARD.encode(der)).is_err());
}

#[tokio::test]
async fn an_rsa_key_refuses_an_ecdsa_signed_tree_head() {
    let log = StubLog::start(vec![vec![1]]).await;
    let http = crate::benchmarks::client().unwrap();
    let client = LogClient::new(http, Url::parse(&log.url).unwrap(), LogKey::parse(RSA_SPKI).unwrap());
    let err = client.tree_head().await.unwrap_err();
    assert!(format!("{err:#}").contains("not the log key's"), "{err:#}");
}

/// A tree head signed with a 2048-bit RSA key (PKCS#1 v1.5, SHA-256) by `openssl`; only the public
/// key and this signature are committed.
const RSA_LOG_SPKI: &str = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA2z64qcboOa9hTp+L43a1Fq+5uN86PgFxyDQ8VSsGRsgmPB8E3PYh3TxEhsRrW5ZNZYF8cIzx+7n5siiqUu7IxJv4Hc/0fhszvXHuPSd64tNFvUQf8BNU3CPAhwGQPueP06r24CFI75aW/U1yx0LnrDKHDl9q8dWQnrqnOTafW+xvjV6q2lim6innA/69L1gBXNUiO2E+SurUlY394ZqHSCJWyROPuP8AjeVYRDDyNHMgtQ91upFuHLNOgyXnugjHRIv8w5Rk9xv56gXe4Rs82Fnj+ymN8f8EQoaizswFLGu8qRyMuBVLv4EapVkisYdpV5uUSjg8/yaZ80UMfIMxsQIDAQAB";
const RSA_SIGNED_HEAD: &str = r#"{"tree_size": 42, "timestamp": 1700000000000, "sha256_root_hash": "OQUPGfY+a2dhOMOXWuEmjc15ecMvvwLBV+HsW4NSZuY=", "tree_head_signature": "BAEBAJ18MbtmtFpUHUgmF0Yf9C7dommckiR5v7/MF1vQ8Mir/oe8du5gErGBWvOzKqlmoIUq0IzOdcQI3ibTsfkflcZEWCi4spwbN6hrwuJXdg4iFIYrauw29rU9tzOH9nNdJzRw25NguPoKW0DNkiA2tN1mwr5sO4SOdtZ6fzrLZe7XUKEf2sKTbZ36l3uhQ/RUIYuetpMGlxb6hppBUYjUaWqEhvIF9GSJGjPpqi/0lUVA71/FfoQF53I7SCEi9FnykAnHK3r34j/0ycZTkC7gFbkYVfpwBfSEOclIHEL8EPprakIEPxEH0/SzCndkjUqbXYwDFhIwK+pIy8uAgnwz+wI="}"#;

async fn rsa_tree_head_of(head: Value) -> anyhow::Result<crate::ct_watch::log_client::TreeHead> {
    use axum::Json;
    let app = Router::new().route("/ct/v1/get-sth", get(move || async move { Json(head.clone()) }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = crate::benchmarks::client().unwrap();
    let client = LogClient::new(http, Url::parse(&url).unwrap(), LogKey::parse(RSA_LOG_SPKI).unwrap());
    let head = client.tree_head().await;
    server.abort();
    head
}

#[tokio::test]
async fn an_rsa_signed_tree_head_verifies_and_a_changed_one_does_not() {
    let signed: Value = serde_json::from_str(RSA_SIGNED_HEAD).unwrap();
    let head = rsa_tree_head_of(signed.clone()).await.unwrap();
    assert_eq!((head.tree_size, head.timestamp_ms), (42, 1_700_000_000_000));

    let mut bigger = signed.clone();
    bigger["tree_size"] = json!(43);
    assert!(rsa_tree_head_of(bigger).await.is_err());
    let mut signature = STANDARD.decode(signed["tree_head_signature"].as_str().unwrap()).unwrap();
    *signature.last_mut().unwrap() ^= 1;
    let mut forged = signed;
    forged["tree_head_signature"] = json!(STANDARD.encode(signature));
    assert!(rsa_tree_head_of(forged).await.is_err());
}
