//! The harness for the poll-flow tests: loopback stand-ins for a log and a node, a controllable
//! clock, and a watcher on a scratch directory.

use super::*;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::alerts::{AlertLine, Severity};
use crate::ct_watch::{Clock, Watcher, config::WatchConfig};
use crate::tee::{AttestationBundle, MockAttestor, attest_bundle};
use crate::test_support::scratch_dir;

pub(super) const T0: u64 = 1_700_000_000_000;
pub(super) const GRACE_MS: u64 = 600_000;
pub(super) const DOMAIN: &str = "api.example.com";

/// The node's `/v1/attestation`: a status and a body, replaceable.
pub(super) struct StubNode {
    pub(super) url: String,
    pub(super) answer: Arc<Mutex<(StatusCode, String)>>,
    pub(super) task: JoinHandle<()>,
}

impl StubNode {
    pub(super) async fn start() -> Self {
        let answer = Arc::new(Mutex::new((StatusCode::SERVICE_UNAVAILABLE, String::new())));
        let app =
            Router::new()
                .route(
                    "/v1/attestation",
                    get(|State(answer): State<Arc<Mutex<(StatusCode, String)>>>| async move {
                        answer.lock().unwrap().clone()
                    }),
                )
                .with_state(answer.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, answer, task }
    }

    /// Serves a mock attestation of `tls_key_sha256`.
    pub(super) fn attest(&self, tls_key_sha256: [u8; 32]) {
        let bundle = AttestationBundle {
            enclave_address: [1; 20],
            config_sha256: [2; 32],
            tls_public_key_sha256: tls_key_sha256,
        };
        let attestation = attest_bundle(&MockAttestor, &bundle).unwrap();
        *self.answer.lock().unwrap() = (StatusCode::OK, serde_json::to_string(&attestation).unwrap());
    }

    pub(super) fn serve(&self, status: StatusCode, body: &str) {
        *self.answer.lock().unwrap() = (status, body.to_owned());
    }
}

impl Drop for StubNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) struct Harness {
    pub(super) watcher: Watcher,
    pub(super) log: StubLog,
    pub(super) node: StubNode,
    pub(super) config: String,
    pub(super) time: Arc<AtomicU64>,
}

/// A watcher on `name`'s scratch directory against a log holding `leaves` and an unattested node.
pub(super) async fn harness(name: &str, leaves: Vec<Vec<u8>>, start_index: Option<u64>, allow_mock: bool) -> Harness {
    let dir = scratch_dir(name);
    std::fs::create_dir_all(&dir).unwrap();
    let log = StubLog::start(leaves).await;
    let node = StubNode::start().await;
    std::fs::write(
        dir.join("release.toml"),
        format!(
            "name = \"test\"\nlaunch_digest = \"0x{}\"\npolicy = \"0x30000\"\n[minimum_tcb]\nboot_loader = 0\ntee = 0\nsnp = 0\nmicrocode = 0\n[config_hashes]\n\"node.toml\" = \"0x{}\"\n",
            "00".repeat(48),
            "00".repeat(32)
        ),
    )
    .unwrap();
    let start = start_index.map(|i| format!("start_index = {i}")).unwrap_or_default();
    let config = format!(
        r#"
domain = "{DOMAIN}"
state_file = "{dir}/state.json"
attestation_interval = "1m"
log_interval = "1m"
[node]
url = "{node}"
release = "{dir}/release.toml"
processor = "Milan"
[[logs]]
name = "log"
url = "{log}"
public_key = "{key}"
{start}
[logging]
level = "info"
[metrics]
listen = "127.0.0.1:0"
[alerts]
episode_quiet = "1h"
[alerts.outbox]
dir = "{dir}/alerts"
max_pages = 10
max_bytes = 1000
max_age = "1h"
"#,
        dir = dir.display(),
        node = node.url,
        log = log.url,
        key = log.key.spki_b64(),
    );
    let time = Arc::new(AtomicU64::new(T0));
    let watcher = open(&config, allow_mock, clock(&time));
    Harness { watcher, log, node, config, time }
}

/// A clock that reads `time`.
pub(super) fn clock(time: &Arc<AtomicU64>) -> Clock {
    let time = time.clone();
    Arc::new(move || time.load(Ordering::SeqCst))
}

/// A clock stopped at `now`.
pub(super) fn fixed(now: u64) -> Clock {
    clock(&Arc::new(AtomicU64::new(now)))
}

pub(super) fn open(config: &str, allow_mock: bool, clock: Clock) -> Watcher {
    let cfg: WatchConfig = toml::from_str(config).unwrap();
    cfg.validate(allow_mock).unwrap();
    Watcher::new(&cfg, allow_mock, clock).unwrap()
}

impl Harness {
    /// One watcher pass as the loop makes it: the attestation, the logs, then the decisions.
    pub(super) async fn pass(&mut self, now: u64) {
        self.log.shared.lock().unwrap().sth_timestamp = now;
        self.attestation_at(now).await;
        self.logs_at(now).await;
        self.settle_at(now);
    }

    /// Polls the node's attestation with the clock at `now`.
    pub(super) async fn attestation_at(&mut self, now: u64) {
        self.time.store(now, Ordering::SeqCst);
        self.watcher.poll_attestation().await;
    }

    /// Polls the logs with the clock at `now`; says whether a log was left behind.
    pub(super) async fn logs_at(&mut self, now: u64) -> bool {
        self.time.store(now, Ordering::SeqCst);
        self.watcher.poll_logs().await.unwrap()
    }

    /// Decides the findings with the clock at `now`.
    pub(super) fn settle_at(&mut self, now: u64) {
        self.time.store(now, Ordering::SeqCst);
        self.watcher.settle().unwrap();
    }

    pub(super) fn next_index(&self) -> Option<u64> {
        self.watcher.store.tracker.state.logs.values().next().unwrap().next_index
    }
}

/// The alerts raised since the last call (resolves are not counted) as `(name, severity, key_sha256 field)`.
pub(super) fn alerts(router: &mut crate::alerts::Router) -> Vec<(String, Option<Severity>, Option<String>)> {
    let mut raised: Vec<_> = router
        .take()
        .into_iter()
        .filter(|line| line.severity.is_some())
        .map(|line: AlertLine| {
            let key = line.fields.iter().find(|(name, _)| name == "key_sha256").map(|(_, value)| value.clone());
            (line.name, line.severity, key)
        })
        .collect();
    // The same alert raised again within a pass is one episode to the router.
    raised.dedup();
    raised
}

pub(super) fn fired(name: &str, key: Option<[u8; 32]>) -> (String, Option<Severity>, Option<String>) {
    (name.to_owned(), Some(Severity::Critical), key.map(hex::encode))
}

pub(super) fn get_entries(h: &Harness) -> Vec<String> {
    h.log.shared.lock().unwrap().requests.iter().filter(|r| r.starts_with("get-entries")).cloned().collect()
}
