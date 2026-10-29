use super::*;
use std::net::SocketAddr;
use std::time::Duration;

use engine_core::types::AccountId;
use engine_wire::{Connection, Deployment, Error, FrameVerifier};
use futures_util::StreamExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::api::router;
use crate::api::signing_tests::{parse, state};
use crate::tee::EnclaveKey;
use crate::test_support::{TestSequencer, deposit, dev_config, log_messages, scratch_dir};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const CHALLENGE: &str = "8f0e5c2a9b7d41e6a3c0f19d2e84b7c65a1d0e3f9b2c7a48e6d1f05b3a9c2e71";

#[tokio::test]
async fn a_signed_stream_frames_every_message_from_zero_under_the_challenge() {
    let dir = scratch_dir("signed-stream");
    let node = TestSequencer::start_legacy(&dir, 1024);
    for n in 0..5 {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    let addr = serve(&node, &dir, Duration::from_secs(5)).await;
    let query = format!("from=0&signed=1&challenge={CHALLENGE}");
    let mut ws = connect(addr, &query).await;

    let expected = log_messages(&dir, Offset::start_of(0));
    let mut verifier = FrameVerifier::new(signer(), deployment(), opened(&query));
    let (mut frames, mut messages) = (Vec::new(), Vec::new());
    while messages.len() < expected.len() {
        let frame: SignedFrame = serde_json::from_str(&next_text(&mut ws).await).unwrap();
        assert_eq!(verifier.verify(&frame), Ok(()), "frame {}", frame.n);
        if !is_heartbeat(&frame) {
            messages.push(frame.msg.clone());
        }
        frames.push(frame);
    }
    // Each message is the unsigned stream's, byte for byte, and the frames count from zero.
    assert_eq!(messages, expected);
    assert!(frames.iter().enumerate().all(|(n, frame)| frame.n == n as u64));

    // The same frames fit no connection opened with another challenge or from another start.
    let other_start = format!("after=3:0&signed=1&challenge={CHALLENGE}");
    for other in [Connection { challenge: [0xc5; 32], ..opened(&query) }, opened(&other_start)] {
        let mut verifier = FrameVerifier::new(signer(), deployment(), other);
        assert_eq!(verifier.verify(&frames[0]), Err(Error::WrongSigner));
    }
}

#[tokio::test]
async fn a_quiet_signed_stream_sends_heartbeats_naming_the_log_head() {
    let dir = scratch_dir("signed-heartbeat");
    let node = TestSequencer::start_legacy(&dir, 16);
    let addr = serve(&node, &dir, Duration::from_secs(1)).await;
    let query = format!("signed=1&challenge={CHALLENGE}");
    let mut ws = connect(addr, &query).await;
    let mut verifier = FrameVerifier::new(signer(), deployment(), opened(&query));
    let head = node.manifest.borrow().flushed.expect("genesis is in the log");
    for _ in 0..2 {
        let frame: SignedFrame = serde_json::from_str(&next_text(&mut ws).await).unwrap();
        assert_eq!(verifier.verify(&frame), Ok(()));
        let msg: serde_json::Value = serde_json::from_str(&frame.msg).unwrap();
        assert_eq!(msg, serde_json::json!({ "type": "heartbeat", "head": head.to_string() }));
    }
}

#[tokio::test]
async fn heartbeats_never_cut_short_a_backfill_read() {
    let dir = scratch_dir("signed-backfill");
    let node = TestSequencer::start_legacy(&dir, 1024);
    // More than one backfill batch, read while a heartbeat falls due every millisecond.
    for n in 0..300 {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    let addr = serve(&node, &dir, Duration::from_millis(1)).await;
    let query = format!("from=0&signed=1&challenge={CHALLENGE}");
    let mut ws = connect(addr, &query).await;
    let expected = log_messages(&dir, Offset::start_of(0));
    let mut verifier = FrameVerifier::new(signer(), deployment(), opened(&query));
    let mut messages = Vec::new();
    // Heartbeats keep arriving whether or not the backfill moves, so the whole read has a deadline.
    let read = async {
        while messages.len() < expected.len() {
            let frame: SignedFrame = serde_json::from_str(&next_text(&mut ws).await).unwrap();
            assert_eq!(verifier.verify(&frame), Ok(()), "frame {}", frame.n);
            if !is_heartbeat(&frame) {
                messages.push(frame.msg);
            }
        }
    };
    timeout(Duration::from_secs(60), read).await.expect("the backfill ends while heartbeats go out");
    assert_eq!(messages, expected);
}

#[tokio::test]
async fn the_stream_refuses_a_signed_flag_other_than_1_or_a_missing_or_malformed_challenge() {
    let dir = scratch_dir("signed-params");
    let node = TestSequencer::start_legacy(&dir, 16);
    let addr = serve(&node, &dir, Duration::from_secs(5)).await;
    for query in [
        "signed=0".to_owned(),
        "signed=2".to_owned(),
        "signed=true".to_owned(),
        "signed=1".to_owned(),
        format!("challenge={CHALLENGE}"),
        format!("signed=1&challenge={}", &CHALLENGE[1..]),
        format!("signed=1&challenge=0x{}", &CHALLENGE[2..]),
    ] {
        let err = connect_async(format!("ws://{addr}/v1/stream?{query}")).await.err().unwrap();
        let tokio_tungstenite::tungstenite::Error::Http(response) = err else { panic!("{query}: {err}") };
        assert_eq!(response.status(), 400, "{query}");
    }
}

// ── Helpers ──

/// Serves the API for `node` on a free local port, with signed heartbeats every `heartbeat`.
async fn serve(node: &TestSequencer, dir: &std::path::Path, heartbeat: Duration) -> SocketAddr {
    let cfg = dev_config();
    let mut state = state(node, dir, &cfg.api);
    let vault = cfg.custody.vault.parse().unwrap();
    state.signing = Arc::new(Signing::new(Arc::new(EnclaveKey::test_key()), cfg.custody.chain_id, vault, heartbeat));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await });
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

fn is_heartbeat(frame: &SignedFrame) -> bool {
    frame.msg.starts_with(r#"{"type":"heartbeat","#)
}

/// The published mock enclave key's address, which the dev config signs with.
fn signer() -> [u8; 20] {
    EnclaveKey::test_key().address()
}

/// The connection `GET /v1/stream?<query>` opens under `CHALLENGE`.
fn opened(query: &str) -> Connection {
    Connection::new(parse(CHALLENGE), &format!("/v1/stream?{query}"))
}

/// The dev config's deployment.
fn deployment() -> Deployment {
    let cfg = dev_config();
    Deployment { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse::<AccountId>().unwrap().0 }
}
