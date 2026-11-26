//! A loopback stand-in for the archive's bucket, behind TLS from a test CA: its S3 API takes create-only
//! uploads whose AWS Signature Version 4 it checks against its own copy of the keys, over the headers the
//! request carries, and its public origin serves each object at its key. Switches make it answer every
//! request with `503`, refuse every upload, answer an upload to a key already taken with `409` in place of `412`,
//! and serve other bytes for an object than the bucket holds. Beside it stands a vault whose finalized and latest
//! checkpoints a test sets, and whose reads it can fail.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use reqwest::Url;
use tokio::net::TcpListener;
use tokio::task::AbortHandle;

use engine_core::command::Command;
use engine_core::eip712::B256;

use crate::archive::{Finality, RANGE_SEQS, Store};
use crate::config::{ArchiveSection, SecretValue};
use crate::feeds::tests::rwa_support::TlsFront;
use crate::log::LogEntry;
use crate::test_support::{TestSequencer, log_entries};

/// A vault whose finalized and latest checkpoints a test sets, and whose next reads it can fail.
#[derive(Clone, Default)]
pub(crate) struct Vault(Arc<Mutex<VaultState>>);

#[derive(Default)]
struct VaultState {
    finalized: Option<(u64, String)>,
    /// Its `latestSeq` and `latestRoot`, which a reader of the bucket takes; the uploader reads only finality.
    latest: Option<(u64, String)>,
    failing: u32,
    reads: u32,
}

impl Finality for Vault {
    async fn finalized(&self) -> anyhow::Result<Option<(u64, String)>> {
        let mut vault = self.0.lock().expect("the stand-in vault");
        vault.reads += 1;
        if vault.failing > 0 {
            vault.failing -= 1;
            anyhow::bail!("the stand-in vault is unreachable");
        }
        Ok(vault.finalized.clone())
    }
}

impl Vault {
    /// Finalizes the checkpoint at `seq`, rooted at `root`.
    pub(crate) fn finalize(&self, seq: u64, root: &str) {
        self.0.lock().expect("the stand-in vault").finalized = Some((seq, root.to_owned()));
    }

    /// Takes the checkpoint at `seq`, rooted at `root`, as its latest.
    pub(crate) fn post(&self, seq: u64, root: &str) {
        self.0.lock().expect("the stand-in vault").latest = Some((seq, root.to_owned()));
    }

    /// Its latest checkpoint, as `latestSeq` and `latestRoot` give it.
    pub(crate) fn latest(&self) -> Option<(u64, String)> {
        self.0.lock().expect("the stand-in vault").latest.clone()
    }

    /// Fails the next `reads` reads of the vault.
    pub(crate) fn fail_next(&self, reads: u32) {
        self.0.lock().expect("the stand-in vault").failing = reads;
    }

    /// How many times the vault has been read.
    pub(crate) fn reads(&self) -> u32 {
        self.0.lock().expect("the stand-in vault").reads
    }

    /// Finalizes the checkpoint `checkpoint_past` finds, at the root `node` wrote, and returns its seq.
    pub(crate) async fn finalize_past(&self, node: &TestSequencer, dir: &Path, ranges: u64) -> u64 {
        let (seq, root) = checkpoint_past(node, dir, ranges).await;
        self.finalize(seq, &root);
        seq
    }
}

/// Sequences ticks on `node`, writing the log in `dir`, until a completed segment holds a checkpoint at or past range
/// `ranges`' first seq, and returns that checkpoint's seq and root.
pub(crate) async fn checkpoint_past(node: &TestSequencer, dir: &Path, ranges: u64) -> (u64, String) {
    loop {
        let completed = node.manifest.borrow().unlisted_from().expect("the writer's listing");
        let past = checkpoints(dir).into_iter().find(|(seq, _)| (ranges * RANGE_SEQS..completed).contains(seq));
        if let Some(checkpoint) = past {
            return checkpoint;
        }
        node.submit(Command::Tick).await;
    }
}

/// Each checkpoint of the log in `dir`, its seq and root.
pub(crate) fn checkpoints(dir: &Path) -> Vec<(u64, String)> {
    log_entries(dir)
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some((checkpoint.seq, checkpoint.root)),
            _ => None,
        })
        .collect()
}

/// The key prefix of the test sequencer's log: its chain and vault.
pub(crate) fn test_log() -> String {
    crate::archive::log_prefix(31_337, engine_core::types::AccountId([0; 20]))
}

/// The key of piece `index` of the tail of the test log's checkpoint at `seq` rooted at `root`.
pub(crate) fn tail_key(seq: u64, root: &B256, index: u64) -> String {
    format!("{}{}", crate::archive::tail_prefix(&test_log(), seq, root), crate::archive::range_key("", index))
}

/// The newest checkpoint of the log in `dir`, its seq and root.
pub(crate) fn newest_checkpoint(dir: &Path) -> (u64, B256) {
    let (seq, root) = checkpoints(dir).pop().expect("a checkpoint");
    (seq, root.parse().expect("a checkpoint's root"))
}

/// The stand-in's bucket name and keys.
pub(crate) const BUCKET: &str = "engine-log";
const REGION: &str = "auto";
const KEY_ID: &str = "stand-in-key-id";
const SECRET_KEY: &str = "stand-in-secret-access-key";

/// The stand-in, running until dropped.
pub(crate) struct StandIn {
    /// Its S3 API's and public origin's URL, the same server.
    pub url: Url,
    /// The same server over plain `http`, for a reader whose client trusts only the public roots.
    pub plain: Url,
    /// A client that trusts the stand-in's CA alone.
    pub client: reqwest::Client,
    bucket: Arc<Mutex<Bucket>>,
    tasks: [AbortHandle; 2],
}

/// What the stand-in holds and how it answers.
#[derive(Default)]
pub(crate) struct Bucket {
    pub objects: BTreeMap<String, Vec<u8>>,
    /// Bytes its public origin serves in place of an object's, once the object is there.
    pub served: BTreeMap<String, Vec<u8>>,
    /// Every request answered `503` while set.
    pub down: bool,
    /// Every upload answered `403 AccessDenied`, with a long answer, while set, as a token without write access is.
    pub refuse_uploads: bool,
    /// An upload to a key already taken answered `409 Conflict`, as a concurrent upload may be, while set.
    pub conflict: bool,
    /// Uploads it took, and those it refused because an object stood at the key, other than a checkpoint's tail's.
    pub created: u64,
    pub refused: u64,
    /// Uploads refused for their signature, or for not being create-only.
    pub unsigned: u64,
    /// Requests for an object's existence (`HEAD`), and for an object (`GET`), other than a checkpoint's tail's.
    pub asked: u64,
    pub read: u64,
    /// Uploads of a checkpoint's tail it took, and requests for one's objects.
    pub tail_created: u64,
    pub tail_read: u64,
}

impl Drop for StandIn {
    fn drop(&mut self) {
        self.tasks.iter().for_each(AbortHandle::abort);
    }
}

impl StandIn {
    /// A stand-in on fresh loopback ports.
    pub(crate) async fn start() -> Self {
        let bucket = Arc::new(Mutex::new(Bucket::default()));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let address = listener.local_addr().expect("local addr");
        let plain = format!("ws://{address}");
        let app = Router::new().fallback(answer).with_state(bucket.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.expect("the stand-in serves") });
        let front = TlsFront::new();
        let (tls, relay) = front.relay(&plain).await;
        let url = Url::parse(&tls.replacen("wss://", "https://", 1)).expect("the stand-in's URL");
        let client = reqwest::Client::builder()
            .use_preconfigured_tls((*front.client).clone())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("a client of the stand-in");
        let plain_url = Url::parse(&format!("http://{address}/")).expect("the stand-in's plain URL");
        Self { url, plain: plain_url, client, bucket, tasks: [server.abort_handle(), relay] }
    }

    /// The `[archive]` section that reaches the stand-in, warning after `lag_alert`.
    pub(crate) fn section(&self, lag_alert: std::time::Duration) -> ArchiveSection {
        ArchiveSection {
            endpoint: self.url.clone(),
            bucket: BUCKET.to_owned(),
            region: REGION.to_owned(),
            public_url: self.url.clone(),
            access_key_id_secret: "archive_key_id".to_owned().try_into().expect("a name"),
            secret_access_key_secret: "archive_secret_key".to_owned().try_into().expect("a name"),
            lag_alert,
        }
    }

    /// A store of the stand-in's bucket under key `prefix` with its keys, or with `secret` in place of its secret key.
    pub(crate) fn store_with(&self, prefix: &str, secret: &str) -> Store {
        let section = self.section(std::time::Duration::from_secs(3600));
        let keys = (&SecretValue::test(KEY_ID), &SecretValue::test(secret));
        Store::new(&section, keys, self.client.clone(), prefix.to_owned()).expect("a store of the stand-in")
    }

    /// A store of the stand-in's bucket under key `prefix` with its keys.
    pub(crate) fn store(&self, prefix: &str) -> Store {
        self.store_with(prefix, SECRET_KEY)
    }

    /// Runs `change` on what the stand-in holds.
    pub(crate) fn with<T>(&self, change: impl FnOnce(&mut Bucket) -> T) -> T {
        change(&mut self.bucket.lock().expect("the stand-in's bucket"))
    }
}

/// Answers one request: an upload to `/<bucket>/<key>`, or a read of `/<key>` from the public origin.
async fn answer(
    State(bucket): State<Arc<Mutex<Bucket>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut bucket = bucket.lock().expect("the stand-in's bucket");
    if bucket.down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let path = uri.path().trim_start_matches('/');
    match method {
        Method::PUT => {
            let Some(key) = path.strip_prefix(BUCKET).and_then(|rest| rest.strip_prefix('/')) else {
                return error(StatusCode::NOT_FOUND, "NoSuchBucket");
            };
            if bucket.refuse_uploads {
                let padding = " ".repeat(1 << 20);
                let body = format!("<?xml version=\"1.0\"?><Error><Code>AccessDenied</Code>{padding}</Error>");
                return (StatusCode::FORBIDDEN, body).into_response();
            }
            if !signed(&uri, &headers) {
                bucket.unsigned += 1;
                return error(StatusCode::FORBIDDEN, "SignatureDoesNotMatch");
            }
            let tail = is_tail(key);
            if bucket.objects.contains_key(key) {
                bucket.refused += u64::from(!tail);
                return match bucket.conflict {
                    true => error(StatusCode::CONFLICT, "ConditionalRequestConflict"),
                    false => error(StatusCode::PRECONDITION_FAILED, "PreconditionFailed"),
                };
            }
            match tail {
                true => bucket.tail_created += 1,
                false => bucket.created += 1,
            }
            bucket.objects.insert(key.to_owned(), body.to_vec());
            StatusCode::OK.into_response()
        }
        Method::GET | Method::HEAD => {
            match (is_tail(path), method == Method::HEAD) {
                (true, _) => bucket.tail_read += 1,
                (false, true) => bucket.asked += 1,
                (false, false) => bucket.read += 1,
            }
            let held = bucket.objects.get(path);
            match (held, method == Method::HEAD) {
                (Some(_), true) => StatusCode::OK.into_response(),
                (Some(held), false) => {
                    (StatusCode::OK, bucket.served.get(path).unwrap_or(held).clone()).into_response()
                }
                (None, _) => StatusCode::NOT_FOUND.into_response(),
            }
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

/// Whether `key` names an object of a checkpoint's tail.
fn is_tail(key: &str) -> bool {
    key.contains("/tail/")
}

/// An S3 error answer naming `code`.
fn error(status: StatusCode, code: &str) -> Response {
    (status, format!("<?xml version=\"1.0\"?><Error><Code>{code}</Code><Message>refused</Message></Error>"))
        .into_response()
}

/// Whether the presigned upload `uri`, sent with `headers`, is signed with the stand-in's keys, over every
/// header it names as signed with the value it carries, `If-None-Match: *` among them.
fn signed(uri: &Uri, headers: &HeaderMap) -> bool {
    let Ok(sent) = Url::parse(&format!("https://{}{uri}", headers_host(headers))) else { return false };
    let query: BTreeMap<String, String> = sent.query_pairs().into_owned().collect();
    let field = |name: &str| query.get(name).map(String::as_str);
    let (Some(date), Some(expires), Some(names), Some(signature)) = (
        field("X-Amz-Date"),
        field("X-Amz-Expires").and_then(|expires| expires.parse::<u64>().ok()),
        field("X-Amz-SignedHeaders"),
        field("X-Amz-Signature"),
    ) else {
        return false;
    };
    let Ok(time) = jiff::civil::DateTime::strptime("%Y%m%dT%H%M%SZ", date)
        .and_then(|time| time.to_zoned(jiff::tz::TimeZone::UTC))
        .map(|time| time.timestamp())
    else {
        return false;
    };
    let mut signed_headers = Vec::new();
    for name in names.split(';').filter(|name| *name != "host") {
        let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) else { return false };
        signed_headers.push((name, value));
    }
    if !signed_headers.contains(&("if-none-match", "*")) {
        return false;
    }
    let mut url = sent.clone();
    url.set_query(None);
    let expected = rusty_s3::signing::sign(
        &time,
        rusty_s3::Method::Put,
        url,
        KEY_ID,
        SECRET_KEY,
        None,
        REGION,
        expires,
        std::iter::empty(),
        signed_headers.into_iter(),
    );
    expected.query_pairs().any(|(name, value)| name == "X-Amz-Signature" && value == signature)
}

/// The `Host` a request carries.
fn headers_host(headers: &HeaderMap) -> &str {
    headers.get("host").and_then(|host| host.to_str().ok()).unwrap_or_default()
}
