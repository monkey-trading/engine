//! Tests for the watcher: leaf parsing, the log client, the tracker, the state file, the config,
//! and the whole poll flow against loopback stand-ins for a log and a node.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rcgen::{CertificateParams, DnType, KeyPair, PublicKeyData};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use x509_parser::certificate::X509Certificate;
use x509_parser::prelude::FromDer;

mod bounds;
mod config;
mod flow;
mod harness;
mod leaf;
mod log_client;
mod state;
mod tracker;

#[test]
fn without_a_critical_webhook_only_a_mock_run_may_start() {
    use crate::ct_watch::ensure_pages_critical;
    assert!(ensure_pages_critical(true, false).is_ok());
    assert!(ensure_pages_critical(false, true).is_ok());
    assert!(ensure_pages_critical(false, false).is_err());
}

/// A production build does not run against a mock node, whatever the config says.
#[cfg(feature = "production")]
#[tokio::test]
async fn production_build_refuses_allow_mock() {
    let args = crate::ct_watch::CtWatchArgs { config: "missing.toml".into(), allow_mock: true };
    let err = crate::ct_watch::run(args).await.unwrap_err();
    assert!(format!("{err:#}").contains("refused in a production build"), "{err:#}");
}

/// The DER prefix of a SubjectPublicKeyInfo holding an uncompressed P-256 point.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce,
    0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

/// A certificate, as DER, and the hash of its key.
pub(super) struct TestCert {
    pub der: Vec<u8>,
    pub key_sha256: [u8; 32],
}

/// A self-signed certificate for `names` (subject alternative names) and `common_name`, under a
/// fresh key. The issuer is the common name too.
pub(super) fn cert(names: &[&str], common_name: &str) -> TestCert {
    let key = KeyPair::generate().expect("a key");
    let mut params = CertificateParams::new(names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>()).expect("params");
    params.distinguished_name.push(DnType::CommonName, common_name);
    let cert = params.self_signed(&key).expect("a certificate");
    TestCert { der: cert.der().to_vec(), key_sha256: Sha256::digest(key.subject_public_key_info()).into() }
}

/// A `MerkleTreeLeaf` for an X.509 entry.
pub(super) fn x509_leaf(timestamp_ms: u64, der: &[u8]) -> Vec<u8> {
    leaf_prefix(timestamp_ms, 0).into_iter().chain(opaque24(der)).chain([0, 0]).collect()
}

/// A `MerkleTreeLeaf` for a precertificate entry holding `der`'s TBSCertificate.
pub(super) fn precert_leaf(timestamp_ms: u64, der: &[u8]) -> Vec<u8> {
    let (_, cert) = X509Certificate::from_der(der).expect("a certificate");
    let tbs = cert.tbs_certificate.as_ref();
    leaf_prefix(timestamp_ms, 1).into_iter().chain([7; 32]).chain(opaque24(tbs)).chain([0, 0]).collect()
}

fn leaf_prefix(timestamp_ms: u64, entry_type: u16) -> Vec<u8> {
    [&[0u8, 0][..], &timestamp_ms.to_be_bytes(), &entry_type.to_be_bytes()].concat()
}

fn opaque24(bytes: &[u8]) -> Vec<u8> {
    let len = bytes.len();
    [&[(len >> 16) as u8, (len >> 8) as u8, len as u8][..], bytes].concat()
}

/// An ECDSA P-256 log key that signs tree heads.
pub(super) struct TestLogKey {
    pair: EcdsaKeyPair,
}

impl TestLogKey {
    pub(super) fn new() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).expect("a key");
        Self { pair: EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).expect("a key") }
    }

    /// The base64 SubjectPublicKeyInfo a log list publishes.
    pub(super) fn spki_b64(&self) -> String {
        STANDARD.encode([&P256_SPKI_PREFIX[..], self.pair.public_key().as_ref()].concat())
    }

    /// A `get-sth` body for a tree of `tree_size` entries at `timestamp`.
    pub(super) fn tree_head(&self, timestamp: u64, tree_size: u64) -> Value {
        let root = [9u8; 32];
        let message = [&[0u8, 1][..], &timestamp.to_be_bytes(), &tree_size.to_be_bytes(), &root].concat();
        let signature = self.pair.sign(&SystemRandom::new(), &message).expect("a signature");
        let signed = [&[4u8, 3][..], &(signature.as_ref().len() as u16).to_be_bytes(), signature.as_ref()].concat();
        json!({
            "tree_size": tree_size,
            "timestamp": timestamp,
            "sha256_root_hash": STANDARD.encode(root),
            "tree_head_signature": STANDARD.encode(signed),
        })
    }
}

/// How the stand-in log misbehaves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Fault {
    #[default]
    None,
    /// Signs tree heads with a key the config does not name.
    WrongKey,
    /// Answers `get-entries` with one entry more than asked for.
    TooManyEntries,
    /// Answers `get-entries` with none.
    NoEntries,
    /// Answers `get-entries` with a body over the size bound.
    HugeBody,
    /// Answers `get-entries` with entries that are not base64.
    NotBase64,
    /// Answers `get-entries` with something that is not JSON.
    NotJson,
    /// Answers with a server error.
    ServerError,
    /// Answers `get-sth` only after ten seconds.
    SlowHead,
}

/// A loopback RFC 6962 log serving `leaves`, at most `per_call` entries a call.
pub(super) struct StubLog {
    pub url: String,
    pub key: Arc<TestLogKey>,
    pub shared: Arc<Mutex<StubLogState>>,
    task: JoinHandle<()>,
}

pub(super) struct StubLogState {
    pub leaves: Vec<Vec<u8>>,
    pub per_call: usize,
    pub fault: Fault,
    /// The signed timestamp of the tree head.
    pub sth_timestamp: u64,
    /// Signs this tree size instead of the number of leaves.
    pub tree_size: Option<u64>,
    pub requests: Vec<String>,
    other_key: TestLogKey,
}

#[derive(Deserialize)]
struct Range {
    start: usize,
    end: usize,
}

impl StubLog {
    pub(super) async fn start(leaves: Vec<Vec<u8>>) -> Self {
        let key = Arc::new(TestLogKey::new());
        let shared = Arc::new(Mutex::new(StubLogState {
            leaves,
            per_call: 2,
            fault: Fault::None,
            sth_timestamp: 1_700_000_000_000,
            tree_size: None,
            requests: Vec::new(),
            other_key: TestLogKey::new(),
        }));
        let app = Router::new()
            .route("/ct/v1/get-sth", get(sth))
            .route("/ct/v1/get-entries", get(entries))
            .with_state((key.clone(), shared.clone()));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let url = format!("http://{}", listener.local_addr().expect("an address"));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.expect("serving") });
        Self { url, key, shared, task }
    }

    pub(super) fn push(&self, leaf: Vec<u8>) {
        self.shared.lock().unwrap().leaves.push(leaf);
    }

    pub(super) fn set_fault(&self, fault: Fault) {
        self.shared.lock().unwrap().fault = fault;
    }
}

impl Drop for StubLog {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type StubState = State<(Arc<TestLogKey>, Arc<Mutex<StubLogState>>)>;

async fn sth(State((key, shared)): StubState) -> impl IntoResponse {
    let slow = {
        let mut log = shared.lock().unwrap();
        log.requests.push("get-sth".into());
        log.fault == Fault::SlowHead
    };
    if slow {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
    let log = shared.lock().unwrap();
    if log.fault == Fault::ServerError {
        return (StatusCode::INTERNAL_SERVER_ERROR, String::new());
    }
    let signer = if log.fault == Fault::WrongKey { &log.other_key } else { &*key };
    (StatusCode::OK, signer.tree_head(log.sth_timestamp, log.tree_size.unwrap_or(log.leaves.len() as u64)).to_string())
}

async fn entries(State((_, shared)): StubState, Query(range): Query<Range>) -> impl IntoResponse {
    let mut log = shared.lock().unwrap();
    log.requests.push(format!("get-entries {}-{}", range.start, range.end));
    let end = range.end.min(range.start + log.per_call - 1).min(log.leaves.len().saturating_sub(1));
    let leaf = |i: usize| json!({ "leaf_input": STANDARD.encode(&log.leaves[i]), "extra_data": "" });
    let mut list: Vec<Value> = (range.start..=end).map(leaf).collect();
    match log.fault {
        Fault::TooManyEntries => list.extend((range.start..=range.end + 1).map(|_| leaf(0))),
        Fault::NoEntries => list.clear(),
        Fault::NotBase64 => list = vec![json!({ "leaf_input": "!!not base64!!", "extra_data": "" })],
        Fault::HugeBody => list = vec![json!({ "leaf_input": "", "extra_data": "A".repeat(33 * 1024 * 1024) })],
        Fault::NotJson => return (StatusCode::OK, "<html>no</html>".to_owned()),
        Fault::ServerError => return (StatusCode::BAD_GATEWAY, String::new()),
        _ => {}
    }
    (StatusCode::OK, json!({ "entries": list }).to_string())
}
