//! `GET /v1/notice` and the signed stream's `notice` frames against a real sequencer on the latest
//! protocol: the notice at the start of a connection, again on each change, and nothing between.

use std::net::SocketAddr;
use std::sync::Arc;

use engine_core::command::{AdminCommand, Command};
use engine_wire::{Connection, Deployment, FrameVerifier, SignedFrame};
use futures_util::StreamExt;
use k256::ecdsa::SigningKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use super::*;
use crate::admin::sign;
use crate::api::signing_tests::{parse, state};
use crate::api::{Signing, router};
use crate::clock::test_clock::TestClock;
use crate::tee::EnclaveKey;
use crate::test_support::{TestSequencer, dev_config, scratch_dir, trusted};

const CHALLENGE: &str = "8f0e5c2a9b7d41e6a3c0f19d2e84b7c65a1d0e3f9b2c7a48e6d1f05b3a9c2e71";
const QUERY: &str = "signed=1&notice=1&challenge=8f0e5c2a9b7d41e6a3c0f19d2e84b7c65a1d0e3f9b2c7a48e6d1f05b3a9c2e71";
/// The dev operator's key, whose public key `config/dev.toml` names.
const DEV_KEY: [u8; 32] = [0x42; 32];

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The next verified `notice` frame's message.
async fn next_notice(ws: &mut Ws, verifier: &mut FrameVerifier) -> serde_json::Value {
    loop {
        let WsMessage::Text(text) =
            timeout(Duration::from_secs(10), ws.next()).await.expect("a frame").unwrap().unwrap()
        else {
            continue;
        };
        let frame: SignedFrame = serde_json::from_str(&text).unwrap();
        assert_eq!(verifier.verify(&frame), Ok(()), "frame {}", frame.n);
        let msg: serde_json::Value = serde_json::from_str(&frame.msg).unwrap();
        if msg["type"] == "notice" {
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

/// Sequences the operator's signed `command` with `nonce`, which must apply.
async fn operate(node: &TestSequencer, nonce: u64, command: AdminCommand) {
    let key = SigningKey::from_slice(&DEV_KEY).unwrap();
    let expires_at = crate::clock::now_ms() + 60_000;
    let request = sign(&key, &dev_config().operator().unwrap(), command, nonce, expires_at);
    let events = node.submit(Command::SignedAdmin(Box::new(request))).await;
    assert_eq!(events.last(), Some(&engine_core::event::Event::AdminApplied), "{events:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_notice_is_served_and_streamed_in_signed_frames_at_the_start_and_on_each_change() {
    let dir = scratch_dir("notice-api");
    let node = TestSequencer::start(&dir, 1024);
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = router(app).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await });

    assert_eq!(get(addr, "/v1/notice").await, (200, serde_json::json!({ "notice": null })));

    let (mut ws, _) = connect_async(format!("ws://{addr}/v1/stream?{QUERY}")).await.unwrap();
    let deployment = Deployment { chain_id: cfg.custody.chain_id, vault: vault.0 };
    let connection = Connection::new(parse(CHALLENGE), &format!("/v1/stream?{QUERY}"));
    let mut verifier = FrameVerifier::new(EnclaveKey::test_key().address(), deployment, connection);
    assert_eq!(next_notice(&mut ws, &mut verifier).await["notice"], serde_json::Value::Null);
    // Quiet while nothing changes.
    assert!(timeout(Duration::from_millis(600), next_notice(&mut ws, &mut verifier)).await.is_err());

    let text = "Security maintenance: withdrawals are paused.";
    operate(&node, 0, AdminCommand::SetNotice { text: text.into() }).await;
    let set = next_notice(&mut ws, &mut verifier).await;
    assert_eq!(set["notice"]["text"], text);
    assert!(set["notice"]["set_at_ms"].as_i64().is_some_and(|at| at > 0), "{set}");
    assert_eq!(get(addr, "/v1/notice").await.1["notice"]["text"], text);

    operate(&node, 1, AdminCommand::ClearNotice).await;
    assert_eq!(next_notice(&mut ws, &mut verifier).await["notice"], serde_json::Value::Null);
    assert_eq!(get(addr, "/v1/notice").await.1, serde_json::json!({ "notice": null }));

    // A connection opened with the notice in force is told at its start.
    operate(&node, 2, AdminCommand::SetNotice { text: text.into() }).await;
    let (mut late, _) = connect_async(format!("ws://{addr}/v1/stream?{QUERY}")).await.unwrap();
    let connection = Connection::new(parse(CHALLENGE), &format!("/v1/stream?{QUERY}"));
    let mut verifier = FrameVerifier::new(EnclaveKey::test_key().address(), deployment, connection);
    assert_eq!(next_notice(&mut late, &mut verifier).await["notice"]["text"], text);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_notice_subscription_needs_a_signed_connection() {
    let dir = scratch_dir("notice-api-unsigned");
    let node = TestSequencer::start(&dir, 1024);
    let cfg = dev_config();
    let app = state(&node, &dir, &cfg.api);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = router(app).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await });
    for query in
        ["notice=1", "notice=2&signed=1&challenge=8f0e5c2a9b7d41e6a3c0f19d2e84b7c65a1d0e3f9b2c7a48e6d1f05b3a9c2e71"]
    {
        let refused = connect_async(format!("ws://{addr}/v1/stream?{query}")).await;
        assert!(refused.is_err(), "{query}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_notice_subscription_holds_no_sequencer_sender_between_reads() {
    let dir = scratch_dir("notice-api-senders");
    let node = TestSequencer::start(&dir, 1024);
    let cfg = dev_config();
    let app = state(&node, &dir, &cfg.api);
    let (held, streams) = (app.tx.strong_count(), app.stream.strong_count());
    let mut feed = NoticeFeed::new(&app, Duration::from_millis(20));
    assert_eq!(app.tx.strong_count(), held, "a subscription's state carries no sender of its own");
    assert_eq!(app.stream.strong_count(), streams, "nor a live log sender, which would hold the stream open");
    // A read in flight holds one, and the first message is the notice.
    assert_eq!(feed.next().await.len(), 1);
    assert_eq!(app.tx.strong_count(), held, "nor does it keep one once the read is done");
}
