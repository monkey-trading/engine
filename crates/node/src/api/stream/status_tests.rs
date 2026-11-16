//! `GET /v1/status` and the signed stream's `status` frames against a real sequencer.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::price::SourceQuote;
use engine_wire::{Connection, Deployment, FrameVerifier, SignedFrame};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use super::*;
use crate::api::signing_tests::{parse, state};
use crate::api::{Signing, router};
use crate::clock::test_clock::TestClock;
use crate::status::Status;
use crate::tee::EnclaveKey;
use crate::test_support::{TestSequencer, dev_config, scratch_dir, trusted};

const CHALLENGE: &str = "8f0e5c2a9b7d41e6a3c0f19d2e84b7c65a1d0e3f9b2c7a48e6d1f05b3a9c2e71";
const QUERY: &str = "signed=1&status=1&challenge=8f0e5c2a9b7d41e6a3c0f19d2e84b7c65a1d0e3f9b2c7a48e6d1f05b3a9c2e71";

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The next verified `status` frame's message.
async fn next_status(ws: &mut Ws, verifier: &mut FrameVerifier) -> serde_json::Value {
    loop {
        let WsMessage::Text(text) =
            timeout(Duration::from_secs(10), ws.next()).await.expect("a frame").unwrap().unwrap()
        else {
            continue;
        };
        let frame: SignedFrame = serde_json::from_str(&text).unwrap();
        assert_eq!(verifier.verify(&frame), Ok(()), "frame {}", frame.n);
        let msg: serde_json::Value = serde_json::from_str(&frame.msg).unwrap();
        if msg["type"] == "status" {
            return msg;
        }
    }
}

/// A plain GET of `path`: the status code and the JSON body.
async fn get(addr: SocketAddr, path: &str) -> (u16, serde_json::Value) {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    conn.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(10), conn.read_to_end(&mut response)).await.expect("a response").unwrap();
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").expect("headers end");
    let head = String::from_utf8_lossy(&response[..split]).to_lowercase();
    let code = head.split(' ').nth(1).unwrap().parse().unwrap();
    (code, serde_json::from_slice(&response[split + 4..]).unwrap_or_default())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_status_is_served_and_streamed_in_signed_frames_that_follow_what_holds_the_sequencer() {
    let dir = scratch_dir("status-api");
    let node = TestSequencer::start_legacy(&dir, 1024);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    let quotes = ["a", "b"]
        .map(|source| SourceQuote { source: source.into(), mid: Fixed::from_int(100_000), ts: now, contract: None })
        .to_vec();
    node.submit(Command::Prices { market: 1, quotes }).await;
    let cfg = dev_config();
    let mut app = state(&node, &dir, &cfg.api);
    let vault = cfg.custody.vault.parse().unwrap();
    app.signing = Arc::new(Signing::new(
        Arc::new(EnclaveKey::test_key()),
        cfg.custody.chain_id,
        vault,
        Duration::from_secs(3600),
        trusted(Arc::new(TestClock::default())),
    ));
    let gauge = prometheus_client::metrics::gauge::Gauge::default();
    app.status = Status { log_sync_held: Some(gauge.clone()), ..Default::default() };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = router(app).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await });

    let (code, read) = get(addr, "/v1/status").await;
    assert_eq!((code, &read["takes_orders"], &read["holds"]), (200, &serde_json::json!(true), &serde_json::json!([])));
    assert!(
        read["venues"].is_array() && read["unpriced_markets"].is_array() && read["trusted_time"]["state"].is_string()
    );

    let (mut ws, _) = connect_async(format!("ws://{addr}/v1/stream?{QUERY}")).await.unwrap();
    let deployment = Deployment { chain_id: cfg.custody.chain_id, vault: vault.0 };
    let connection = Connection::new(parse(CHALLENGE), &format!("/v1/stream?{QUERY}"));
    let mut verifier = FrameVerifier::new(EnclaveKey::test_key().address(), deployment, connection);
    let first = next_status(&mut ws, &mut verifier).await;
    assert_eq!((&first["takes_orders"], &first["holds"]), (&serde_json::json!(true), &serde_json::json!([])));
    // Quiet while nothing changes.
    assert!(timeout(Duration::from_millis(600), next_status(&mut ws, &mut verifier)).await.is_err());

    gauge.set(1);
    let held = next_status(&mut ws, &mut verifier).await;
    assert_eq!(held["takes_orders"], false);
    assert_eq!(held["holds"][0]["cause"], "log_sync_held");
    assert_eq!(get(addr, "/v1/status").await.1["holds"][0]["cause"], "log_sync_held");
    gauge.set(0);
    assert_eq!(next_status(&mut ws, &mut verifier).await["takes_orders"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_status_subscription_needs_a_signed_connection() {
    let dir = scratch_dir("status-api-unsigned");
    let node = TestSequencer::start_legacy(&dir, 1024);
    let cfg = dev_config();
    let app = state(&node, &dir, &cfg.api);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = router(app).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await });
    for query in
        ["status=1", "status=2&signed=1&challenge=8f0e5c2a9b7d41e6a3c0f19d2e84b7c65a1d0e3f9b2c7a48e6d1f05b3a9c2e71"]
    {
        let refused = connect_async(format!("ws://{addr}/v1/stream?{query}")).await;
        assert!(refused.is_err(), "{query}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_status_subscription_holds_no_sequencer_sender_between_reads() {
    let dir = scratch_dir("status-api-senders");
    let node = TestSequencer::start_legacy(&dir, 1024);
    let cfg = dev_config();
    let app = state(&node, &dir, &cfg.api);
    let held = app.tx.strong_count();
    let mut feed = StatusFeed::new(&app, Duration::from_millis(20));
    assert_eq!(app.tx.strong_count(), held, "a subscription's state carries no sender of its own");
    // A read in flight holds one, and the first message is the state.
    assert_eq!(feed.next().await.len(), 1);
    assert_eq!(app.tx.strong_count(), held, "nor does it keep one once the read is done");
}
