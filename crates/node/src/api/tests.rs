use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::Path;
use std::time::Duration;

use engine_core::command::{Command, WithdrawalRequest};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::clock::test_clock::TestClock;
use crate::clock::{Clock, now_ms};
use crate::config::RateLimit;
use crate::history::{HistoryEvent, INDEX_FILE, account_of};
use crate::log::{LogEntry, MANIFEST, Manifest, Offset, SegmentedLog, segment_seq};
use crate::sequencer::fixture_tests::finished_genesis;
use crate::sequencer::replay_path_with_trust;
use crate::tee::{Attestation, AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{
    SEGMENT_COMMANDS, TRADER, TestHistory, TestSequencer, deposit, dev_config, log_messages, logged_history,
    scratch_dir, withdraw,
};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The protocol reference log: deposits, trades, rollover financing, a close-out, and withdrawals.
const REFERENCE_LOG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v2.jsonl");

/// The feature's dev scenario: a subscriber reads from offset zero, disconnects between two
/// messages of one seq, resumes after the last offset it received, and sees every message once;
/// then the archive downloads and replays.
#[tokio::test]
async fn dev_scenario_resume_between_messages_of_one_seq_and_replay_the_archive() {
    let dir = scratch_dir("dev-scenario");
    let node = TestSequencer::start_legacy(&dir, 1024);
    let addr = serve(&node, &dir).await;
    node.submit(deposit(100, "dep-1")).await;
    let seq = node.tick_to_checkpoint().await;
    node.submit(withdraw(40)).await;

    let mut received: Vec<String> = Vec::new();
    let mut ws = connect(addr, "from=0").await;
    while received.last().is_none_or(|m| offset(m) != Offset::start_of(seq)) {
        received.push(next_text(&mut ws).await);
    }
    // The withdrawal and checkpoint of `seq` were sent but never read.
    drop(ws);

    node.submit(withdraw(10)).await;
    for n in 0..SEGMENT_COMMANDS {
        node.submit(deposit(1, &format!("dep-offline-{n}"))).await;
    }
    let mut ws = connect(addr, &format!("after={seq}:0")).await;
    for n in 0..SEGMENT_COMMANDS {
        node.submit(deposit(1, &format!("dep-live-{n}"))).await;
    }
    let expected = log_messages(&dir, Offset::start_of(0));
    while received.len() < expected.len() {
        received.push(next_text(&mut ws).await);
    }
    assert_eq!(received, expected);

    let (status, body) = get(addr, &format!("/v1/archive/{MANIFEST}")).await;
    assert_eq!(status, 200);
    let archive = scratch_dir("dev-scenario-archive");
    fs::create_dir_all(&archive).unwrap();
    fs::write(archive.join(MANIFEST), &body).unwrap();
    let manifest = Manifest::load(&archive).unwrap();
    assert!(manifest.segments.len() >= 3, "{manifest:?}");
    for listed in manifest.segments.iter() {
        let (status, segment) = get(addr, &format!("/v1/archive/{}", listed.file)).await;
        assert_eq!(status, 200);
        fs::write(archive.join(&listed.file), segment).unwrap();
    }
    let replayed = replay_path_with_trust(
        &archive,
        None,
        Some(engine_core::types::AccountId(crate::tee::EnclaveKey::test_key().address())),
    )
    .unwrap();
    assert_eq!(replayed.engine.last_seq(), manifest.segments.last().map(|s| s.last_seq));
    assert!(replayed.checkpoints > 0);
}

#[tokio::test]
async fn the_archive_serves_the_enclave_manifest_and_completed_segments_only() {
    let dir = scratch_dir("archive-routes");
    let node = TestSequencer::start_legacy(&dir, 16);
    let addr = serve(&node, &dir).await;
    for n in 0..2 * SEGMENT_COMMANDS {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    let manifest = node.manifest.borrow().clone();
    let completed = &manifest.segments[0];
    let (status, body) = get(addr, &format!("/v1/archive/{}", completed.file)).await;
    assert_eq!((status, body), (200, fs::read(dir.join(&completed.file)).unwrap()));

    // The host's copy of the manifest is not what gets served or trusted.
    let listed = fs::read(dir.join(MANIFEST)).unwrap();
    let forged = br#"{"file":"/etc/hosts","first_seq":999,"last_seq":999,"bytes":1,"sha256":"00"}"#;
    fs::write(dir.join(MANIFEST), [listed.as_slice(), forged, b"\n"].concat()).unwrap();
    assert_eq!(get(addr, &format!("/v1/archive/{MANIFEST}")).await, (200, listed));

    let open_segment = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .find(|name| segment_seq(name).is_some_and(|seq| manifest.lookup(seq).expect("trusted lookup").is_none()))
        .unwrap();
    for path in
        [format!("/v1/archive/{open_segment}"), "/v1/archive/nothing.jsonl".into(), "/v1/archive/%2Fetc%2Fhosts".into()]
    {
        assert_eq!(get(addr, &path).await.0, 404, "{path}");
    }

    fs::remove_file(dir.join(&manifest.segments[1].file)).unwrap();
    assert_eq!(get(addr, &format!("/v1/archive/{}", manifest.segments[1].file)).await.0, 500);
}

#[tokio::test]
async fn the_archive_serves_only_what_the_enclave_manifest_lists() {
    let dir = scratch_dir("archive-empty");
    let node = TestSequencer::start_legacy(&dir, 16);
    // Segment 0 is complete on disk, but this manifest lists nothing.
    let addr = serve_with(&node, &dir, watch::channel(Manifest::default()).1).await;
    assert_eq!(get(addr, &format!("/v1/archive/{MANIFEST}")).await, (200, Vec::new()));
    assert_eq!(get(addr, "/v1/archive/00000000000000000000.jsonl").await.0, 404);
}

/// The account history dev scenario. On a log that deposits, trades, crosses rollovers, closes out,
/// and withdraws, rebuilt into an index from its completed segments, every account's history pages
/// one event at a time. More commands then rotate the log, the node restarts, and history pages
/// again; then again after the index is removed and rebuilt from the log. Each time, every event
/// in the log appears once, newest first.
#[tokio::test]
async fn dev_scenario_page_account_history_one_event_at_a_time_across_a_restart() {
    let (dir, index) = (scratch_dir("history-dev-scenario"), scratch_dir("history-dev-index"));
    let mut log = SegmentedLog::open(&dir, |_| Ok(()))
        .unwrap()
        .into_writer(SEGMENT_COMMANDS, watch::channel(Manifest::default()).0)
        .unwrap();
    let reference = crate::sequencer::fixture_tests::signed_fixture(Path::new(REFERENCE_LOG));
    for entry in &reference {
        log.append(entry).unwrap();
    }
    log.flush().unwrap();
    drop(log);
    let mut accounts: BTreeSet<AccountId> = reference
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(c) => c.events.clone(),
            _ => None,
        })
        .flatten()
        .filter_map(|event| account_of(&event))
        .collect();
    accounts.insert(TRADER);
    let addr = serve_history(Reader::new(index.join(INDEX_FILE), watch::channel(true).1)).await;
    // Trusted time admits stamps near the clock, so the node runs on one set just after the log.
    let last_ts = reference.iter().filter_map(|entry| match entry {
        LogEntry::Command(c) => Some(c.sequenced.ts),
        _ => None,
    });
    let clock: Arc<dyn Clock> = Arc::new(TestClock::at(last_ts.max().unwrap() + 1_000));
    // The node resumes under the genesis the reference log finished, not the dev config's.
    let genesis = finished_genesis(&reference);

    let node = TestSequencer::start_legacy_at(&dir, 1024, clock.clone(), genesis.clone());
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    node.submit(deposit(100, "dep-1")).await;
    node.submit(withdraw(40)).await;
    node.stop().await;
    history.finish().await.unwrap();
    let first = every_history_page(addr, &accounts).await;
    assert_eq!(first, expected_history(&dir, &accounts));
    let kinds: BTreeSet<&str> = first.values().flatten().map(|e| e["type"].as_str().unwrap()).collect();
    for kind in ["deposited", "fill", "financing", "close_out", "withdrawal_approved"] {
        assert!(kinds.contains(kind), "{kind} is missing from {kinds:?}");
    }

    let segments = Manifest::load(&dir).unwrap().segments.len();
    let node = TestSequencer::start_legacy_at(&dir, 1024, clock.clone(), genesis.clone());
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    for n in 0..SEGMENT_COMMANDS {
        node.submit(deposit(1, &format!("dep-rotate-{n}"))).await;
    }
    node.stop().await;
    history.finish().await.unwrap();
    assert!(Manifest::load(&dir).unwrap().segments.len() > segments, "the log rotated");
    let second = every_history_page(addr, &accounts).await;
    assert_eq!(second, expected_history(&dir, &accounts));
    for (account, events) in &first {
        assert!(second[account].ends_with(events), "{account}: events served before the restart changed");
    }
    assert_eq!(second[&TRADER].len(), first[&TRADER].len() + SEGMENT_COMMANDS as usize);

    fs::remove_dir_all(&index).unwrap();
    let node = TestSequencer::start_legacy_at(&dir, 1024, clock.clone(), genesis.clone());
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    node.stop().await;
    history.finish().await.unwrap();
    assert_eq!(every_history_page(addr, &accounts).await, second);
}

#[tokio::test]
async fn history_refuses_a_malformed_address_cursor_kind_or_limit() {
    let (dir, index) = (scratch_dir("history-params"), scratch_dir("history-params-index"));
    let node = TestSequencer::start_legacy(&dir, 16);
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    node.submit(deposit(100, "dep-1")).await;
    node.stop().await;
    history.finish().await.unwrap();
    let addr = serve_history(Reader::new(index.join(INDEX_FILE), watch::channel(true).1)).await;
    let path = format!("/v1/accounts/{TRADER}/history");
    for query in ["limit=200", "limit=1", "kind=deposited", "before=99999:0", "after=0:0"] {
        assert_eq!(get(addr, &format!("{path}?{query}")).await.0, 200, "{query}");
    }
    for query in ["limit=0", "limit=201", "kind=deposit", "before=3", "after=x:1", "since=1:0"] {
        assert_eq!(get(addr, &format!("{path}?{query}")).await.0, 400, "{query}");
    }
    assert_eq!(get(addr, "/v1/accounts/0x1234/history").await.0, 400);
}

#[tokio::test]
async fn history_answers_503_while_catching_up_and_500_when_unreadable() {
    let (dir, index) = (scratch_dir("history-unavailable"), scratch_dir("history-unavailable-index"));
    let node = TestSequencer::start_legacy(&dir, 16);
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    node.submit(deposit(100, "dep-1")).await;
    node.stop().await;
    history.finish().await.unwrap();
    let path = format!("/v1/accounts/{TRADER}/history");
    let catching_up = serve_history(Reader::new(index.join(INDEX_FILE), watch::channel(false).1)).await;
    assert_eq!(get(catching_up, &path).await, (503, br#"{"error":"history is catching up with the log"}"#.to_vec()));
    rusqlite::Connection::open(index.join(INDEX_FILE)).unwrap().execute_batch("DROP TABLE events").unwrap();
    let unreadable = serve_history(Reader::new(index.join(INDEX_FILE), watch::channel(true).1)).await;
    assert_eq!(get(unreadable, &path).await, (500, br#"{"error":"internal error"}"#.to_vec()));
}

#[tokio::test]
async fn the_stream_refuses_after_and_from_together_or_a_malformed_offset() {
    let dir = scratch_dir("stream-params");
    let node = TestSequencer::start_legacy(&dir, 16);
    let addr = serve(&node, &dir).await;
    for query in ["after=1:0&from=0", "after=1", "from=x", "since=0"] {
        let err = connect_async(format!("ws://{addr}/v1/stream?{query}")).await.err().unwrap();
        let tokio_tungstenite::tungstenite::Error::Http(response) = err else { panic!("{query}: {err}") };
        assert_eq!(response.status(), 400, "{query}");
    }
}

#[test]
fn retry_after_is_the_wait_in_whole_seconds_rounded_up() {
    for (wait, header) in [(1500, "2"), (3000, "3"), (1, "1"), (0, "1")] {
        let response = ApiError::RateLimited(Duration::from_millis(wait)).into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[RETRY_AFTER], header, "a wait of {wait}ms");
    }
}

#[tokio::test]
async fn withdrawals_are_refused_while_custody_cannot_pay_them_out() {
    let (state, mut rx) = api_state(false);
    let request = WithdrawalRequest { account: AccountId([0xaa; 20]), amount: Fixed::from_int(5), auth: None };
    let answer = timeout(Duration::from_secs(5), super::withdraw(State(state), None, Json(request)));
    let res = answer.await.expect("the endpoint answers without waiting on the sequencer");
    assert!(matches!(res, Err(ApiError::NotImplemented(_))));
    assert!(rx.try_recv().is_err(), "nothing may reach the sequencer");
}

#[tokio::test]
async fn the_dev_deposit_endpoint_exists_only_under_mock_custody() {
    let (state, mut rx) = api_state(false);
    let answer = timeout(Duration::from_secs(5), dev_deposit(State(state), Json(body())));
    let res = answer.await.expect("the endpoint answers without waiting on the sequencer");
    assert!(matches!(res, Err(ApiError::NotFound)));
    assert!(rx.try_recv().is_err(), "nothing may reach the sequencer");

    let (state, mut rx) = api_state(true);
    let handler = tokio::spawn(dev_deposit(State(state), Json(body())));
    let Some(sequencer::Request::Submit { cmd: Command::Deposit { amount, .. }, reply: Some(reply) }) = rx.recv().await
    else {
        panic!("mock custody must sequence the deposit");
    };
    assert_eq!(amount, Fixed::from_int(5));
    reply.send(vec![]).unwrap();
    assert!(handler.await.unwrap().is_ok());
}

// ── Helpers ──

/// Serves the API for `node`, with `dir` as its log directory, on a free local port.
async fn serve(node: &TestSequencer, dir: &Path) -> SocketAddr {
    serve_with(node, dir, node.manifest.clone()).await
}

/// A mock attestation of a fresh key's address, with zero config and TLS key hashes.
fn mock_attestation() -> Arc<Attestation> {
    let bundle = AttestationBundle {
        enclave_address: EnclaveKey::generate().address(),
        config_sha256: [0; 32],
        tls_public_key_sha256: [0; 32],
    };
    Arc::new(attest_bundle(&MockAttestor, &bundle).unwrap())
}

/// Serves the API for `node`, with `dir` as its log directory and `manifest` as the writer's.
async fn serve_with(node: &TestSequencer, dir: &Path, manifest: watch::Receiver<Manifest>) -> SocketAddr {
    serve_state(ApiState {
        protocol: engine_core::protocol::ProtocolVersion::V19,
        access: None,
        tx: node.tx.clone(),
        stream: node.stream.clone(),
        attestation: mock_attestation(),
        auth: ClientAuth::Unsigned,
        operator: dev_config().operator().unwrap(),
        mock_custody: true,
        payouts_enabled: true,
        clock: now_ms,
        log_dir: dir.to_owned(),
        manifest,
        backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
        limits: Arc::new(ApiLimits::new(&dev_config().api)),
        history: Reader::new(dir.join(INDEX_FILE), watch::channel(true).1),
        emission_budget: None,
        listing: Default::default(),
    })
    .await
}

/// Serves the API with only `history` behind it, so it outlives any sequencer. Its per-client rate
/// lets one client page a long log an event at a time.
async fn serve_history(history: Reader) -> SocketAddr {
    let mut api = dev_config().api;
    let ample = NonZeroU32::new(1_000_000).unwrap();
    api.per_ip = RateLimit { per_second: ample, burst: ample };
    serve_state(ApiState {
        protocol: engine_core::protocol::ProtocolVersion::V19,
        access: None,
        tx: mpsc::channel(1).0,
        stream: broadcast::channel(1).0,
        attestation: mock_attestation(),
        auth: ClientAuth::Unsigned,
        operator: dev_config().operator().unwrap(),
        mock_custody: false,
        payouts_enabled: false,
        clock: now_ms,
        log_dir: PathBuf::new(),
        manifest: watch::channel(Manifest::default()).1,
        backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
        limits: Arc::new(ApiLimits::new(&api)),
        history,
        emission_budget: None,
        listing: Default::default(),
    })
    .await
}

/// Serves `state`'s API on a free local port.
async fn serve_state(state: ApiState) -> SocketAddr {
    let app = router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await });
    addr
}

async fn connect(addr: SocketAddr, query: &str) -> Ws {
    connect_async(format!("ws://{addr}/v1/stream?{query}")).await.unwrap().0
}

/// The next text message, failing the test if none comes within ten seconds.
async fn next_text(ws: &mut Ws) -> String {
    loop {
        let msg = timeout(Duration::from_secs(10), ws.next()).await.expect("a message in time");
        if let WsMessage::Text(text) = msg.expect("stream open").unwrap() {
            return text.to_string();
        }
    }
}

/// Each account's history, paged one event at a time from the newest, following `next`. Each page
/// must hold only events older than the `before` it was asked for, so a page that does not move on
/// fails rather than repeats.
async fn every_history_page(
    addr: SocketAddr,
    accounts: &BTreeSet<AccountId>,
) -> BTreeMap<AccountId, Vec<serde_json::Value>> {
    let mut history = BTreeMap::new();
    for account in accounts {
        let (mut events, mut before): (Vec<serde_json::Value>, Option<Offset>) = (Vec::new(), None);
        loop {
            let query = before.map_or_else(String::new, |before| format!("&before={before}"));
            let (status, body) = get(addr, &format!("/v1/accounts/{account}/history?limit=1{query}")).await;
            assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
            let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let page_events = page["events"].as_array().unwrap();
            assert!(page_events.len() <= 1, "{page}");
            let next: Option<Offset> = page["next"].as_str().map(|next| next.parse().unwrap());
            let cursors = page_events.iter().map(|e| e["cursor"].as_str().unwrap().parse::<Offset>().unwrap());
            let older = |cursor: Offset| before.is_none_or(|before| cursor < before);
            assert!(cursors.chain(next).all(older), "{page} after {before:?}");
            events.extend(page_events.iter().cloned());
            match next {
                Some(next) => before = Some(next),
                None => break,
            }
        }
        history.insert(*account, events);
    }
    history
}

/// Each account's events in the log in `dir`, newest first, as history serves them.
fn expected_history(dir: &Path, accounts: &BTreeSet<AccountId>) -> BTreeMap<AccountId, Vec<serde_json::Value>> {
    let encode = |events: Vec<HistoryEvent>| events.iter().map(|e| serde_json::to_value(e).unwrap()).collect();
    accounts.iter().map(|a| (*a, encode(logged_history(dir, *a)))).collect()
}

fn offset(msg: &str) -> Offset {
    let value: serde_json::Value = serde_json::from_str(msg).unwrap();
    value["offset"].as_str().unwrap().parse().unwrap()
}

/// A plain HTTP/1.1 GET: the status and the body.
async fn get(addr: SocketAddr, path: &str) -> (u16, Vec<u8>) {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    conn.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(10), conn.read_to_end(&mut response)).await.expect("a response in time").unwrap();
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").expect("headers end");
    let head = String::from_utf8_lossy(&response[..split]).to_lowercase();
    assert!(!head.contains("transfer-encoding: chunked"), "{head}");
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, response[split + 4..].to_vec())
}

/// API state over a channel no sequencer reads, so a test sees each request the API sends.
pub(super) fn api_state(mock_custody: bool) -> (ApiState, mpsc::Receiver<sequencer::Request>) {
    let (tx, rx) = mpsc::channel(1);
    let attestation = mock_attestation();
    let (stream, _) = broadcast::channel(1);
    let (_, manifest) = watch::channel(Manifest::default());
    let state = ApiState {
        protocol: engine_core::protocol::ProtocolVersion::V19,
        access: None,
        tx,
        stream,
        attestation,
        auth: ClientAuth::Unsigned,
        operator: dev_config().operator().unwrap(),
        mock_custody,
        payouts_enabled: mock_custody,
        clock: now_ms,
        log_dir: scratch_dir("api-state"),
        manifest,
        backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
        limits: Arc::new(ApiLimits::new(&dev_config().api)),
        history: Reader::new(PathBuf::new(), watch::channel(true).1),
        emission_budget: None,
        listing: Default::default(),
    };
    (state, rx)
}

fn body() -> AmountBody {
    AmountBody { account: AccountId([0xaa; 20]), amount: Fixed::from_int(5) }
}

mod catalog_archive;
