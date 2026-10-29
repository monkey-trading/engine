//! The cost of signed answers, for comparison with `hot_path_benchmark`'s 200 µs p99 command budget:
//! one frame signature, a signed account read, and a live fan-out at `api.max_subscribers`. Signing
//! runs on the API's tasks, never the sequencer's thread. Run it in a quiet CPU window:
//! `cargo test -p engine-node --lib signing_bench -- --ignored --nocapture`.

use super::*;
use std::hint::black_box;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::time::Instant;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::Request as HttpRequest;
use futures_util::StreamExt;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tower::ServiceExt;

use crate::config::RateLimit;
use crate::log::Offset;
use crate::tee::EnclaveKey;
use crate::test_support::{TRADER, TestSequencer, deposit, dev_config, log_messages, scratch_dir, signing};

const CHALLENGE: &str = "8f0e5c2a9b7d41e6a3c0f19d2e84b7c65a1d0e3f9b2c7a48e6d1f05b3a9c2e71";
const LIVE_MESSAGES: usize = 50;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run in a quiet CPU window"]
async fn signed_answer_cost() {
    let mut api = dev_config().api;
    let ample = NonZeroU32::new(1_000_000).unwrap();
    api.per_ip = RateLimit { per_second: ample, burst: ample };
    let subscribers = api.max_subscribers.get();
    let dir = scratch_dir("signing-bench");
    let node = TestSequencer::start_legacy(&dir, 4096);
    node.submit(deposit(100_000, "dep")).await;

    // One frame, over the largest message in the log.
    let msg = log_messages(&dir, Offset::start_of(0)).into_iter().max_by_key(String::len).unwrap();
    let (signing, connection) = (signing(), engine_wire::Connection::new([7; 32], "/v1/stream?signed=1&challenge=07"));
    let sign_frame = |n| drop(black_box(signing.sign_frame(&connection, &[9; 32], n, 1, &msg)));
    let frame = percentiles((0..1000).map(|n| timed(|| sign_frame(n))).collect());
    println!("SIGNED frame msg_bytes={} p50_us={} p99_us={}", msg.len(), frame.0, frame.1);
    let heartbeats = subscribers as f64 / api.heartbeat_interval.as_secs_f64();
    println!(
        "SIGNED fan-out subscribers={subscribers} cpu_us_per_live_message={} heartbeat_signatures_per_s={heartbeats}",
        frame.0 * subscribers as u128
    );

    // A signed account read through the API, and the signature alone.
    let state = crate::api::signing_tests::state(&node, &dir, &api);
    let app = router(state.clone()).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
    let target = format!("/v1/accounts/{TRADER}");
    let mut reads = Vec::new();
    for _ in 0..200 {
        let request = HttpRequest::get(&target).header(engine_wire::CHALLENGE_HEADER, CHALLENGE).body(Body::empty());
        let began = Instant::now();
        let response = app.clone().oneshot(request.unwrap()).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        reads.push(began.elapsed().as_micros());
        assert!(!body.is_empty());
    }
    let body = br#"{"balance":"100000","equity":"100000","margin":"0","positions":{},"orders":{},"toxicity_bps":0}"#;
    let response = engine_wire::Response {
        challenge: [7; 32],
        time_ms: 1,
        head_seq: 1,
        status: 200,
        method: "GET",
        path_and_query: &target,
        request_body: b"",
        body,
    };
    let sign = percentiles((0..1000).map(|_| timed(|| drop(black_box(signing.sign_response(&response))))).collect());
    let reads = percentiles(reads);
    println!(
        "SIGNED get_account p50_us={} p99_us={} signature_p50_us={} signature_p99_us={}",
        reads.0, reads.1, sign.0, sign.1
    );

    // Every subscriber place taken, signed with heartbeats and then unsigned, while live messages go out.
    for signed in [true, false] {
        let elapsed = fan_out(&node, state.clone(), &api, signed).await;
        println!("SIGNED fan-out signed={signed} subscribers={subscribers} messages={LIVE_MESSAGES} wall_ms={elapsed}");
    }
}

// ── Helpers ──

/// Milliseconds from the first of `LIVE_MESSAGES` deposits until `api.max_subscribers` live
/// subscribers, signed with one-second heartbeats or unsigned, have each received the last.
async fn fan_out(node: &TestSequencer, mut state: ApiState, api: &crate::config::ApiSection, signed: bool) -> u128 {
    let cfg = dev_config();
    let vault = cfg.custody.vault.parse().unwrap();
    let interval = Duration::from_secs(1);
    state.signing = Arc::new(Signing::new(Arc::new(EnclaveKey::test_key()), cfg.custody.chain_id, vault, interval));
    // Places of its own, so the run before it need not have closed its subscribers yet.
    state.limits = Arc::new(ApiLimits::new(api));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let query = if signed { format!("signed=1&challenge={CHALLENGE}") } else { String::new() };
    let last = format!("fan-out-{signed}-{}", LIVE_MESSAGES - 1);
    let mut readers = JoinSet::new();
    for _ in 0..api.max_subscribers.get() {
        let (mut ws, _) = connect_async(format!("ws://{addr}/v1/stream?{query}")).await.unwrap();
        let last = last.clone();
        // Live messages, deposits and checkpoints alike, reach each subscriber in log order.
        readers.spawn(async move {
            loop {
                match ws.next().await {
                    Some(Ok(WsMessage::Text(text))) if text.contains(&last) => return,
                    Some(Ok(_)) => {}
                    ended => panic!("the stream ended before the last deposit: {ended:?}"),
                }
            }
        });
    }
    let began = Instant::now();
    for n in 0..LIVE_MESSAGES {
        let events = node.submit(deposit(1, &format!("fan-out-{signed}-{n}"))).await;
        assert!(!events.is_empty(), "deposit {n} was refused");
    }
    let readers = timeout(Duration::from_secs(120), readers.join_all()).await;
    readers.expect("every subscriber receives the last deposit in time");
    let elapsed = began.elapsed().as_millis();
    server.abort();
    elapsed
}

/// Microseconds `f` takes.
fn timed(f: impl FnOnce()) -> u128 {
    let began = Instant::now();
    f();
    began.elapsed().as_micros()
}

/// The p50 and p99 of `samples`.
fn percentiles(mut samples: Vec<u128>) -> (u128, u128) {
    samples.sort_unstable();
    (samples[samples.len() / 2], samples[samples.len() * 99 / 100])
}
