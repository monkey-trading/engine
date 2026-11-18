//! Signed frames from offset zero bound to their opening request, quiet-log heartbeats, a backfill
//! read never cut short by a heartbeat, a connection closed while trusted time does not bound the
//! host clock, and refused parameters.

use super::*;
use std::net::SocketAddr;
use std::time::Duration;

use crate::log::Manifest;
use crate::sequencer::StreamMsg;
use engine_core::types::AccountId;
use engine_wire::{Connection, Deployment, Error, FRAME_VERSION, FrameVerifier, SignedFrame};
use futures_util::StreamExt;
use sha2::Digest;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::api::router;
use crate::api::signing_tests::{parse, state};
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::tee::EnclaveKey;
use crate::test_support::{TestSequencer, deposit, dev_config, log_messages, sample_around, scratch_dir, trusted};
use crate::trusted_time::TrustedClock;

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
async fn a_signed_stream_closes_rather_than_frame_while_trusted_time_does_not_bound_the_host_clock() {
    let dir = scratch_dir("signed-untrusted-time");
    let node = TestSequencer::start_legacy(&dir, 16);
    let clock = Arc::new(TestClock::default());
    let (samples, latest) = watch::channel(None);
    let time = TrustedClock::new(clock.clone(), latest, dev_config().time.max_drift);
    // Hourly heartbeats: after the first, which is due at once, the next frame is a live message.
    let addr = serve_at(&node, &dir, Duration::from_secs(3600), time).await;
    let query = format!("signed=1&challenge={CHALLENGE}");
    // Before the first sample, the first heartbeat is not framed.
    assert_eq!(next_text_or_end(&mut connect(addr, &query).await).await, None);

    samples.send_replace(Some(sample_around(clock.as_ref())));
    let mut ws = connect(addr, &query).await;
    let heartbeat: SignedFrame = serde_json::from_str(&next_text(&mut ws).await).unwrap();
    assert_eq!(FrameVerifier::new(signer(), deployment(), opened(&query)).verify(&heartbeat), Ok(()));
    assert_eq!(heartbeat.time, u64::try_from(clock.now_ms()).unwrap());
    // The host clock skewed past the bound: the next message is not framed.
    clock.skew(3_600_000);
    node.submit(deposit(1, "dep-skewed")).await;
    assert_eq!(next_text_or_end(&mut ws).await, None);
}

/// A connection that asks for heads gets them, bare through a trusted-time halt, and keeps them coming;
/// one that does not gets none, and closes at the halt as any signed connection does.
#[tokio::test]
async fn an_operators_heads_go_out_bare_and_keep_the_connection_through_a_trusted_time_halt() {
    let dir = scratch_dir("signed-bare-heads");
    let node = TestSequencer::start_legacy(&dir, 16);
    let clock = Arc::new(TestClock::default());
    let (_samples, latest) = watch::channel(None);
    let time = TrustedClock::new(clock.clone(), latest, dev_config().time.max_drift);
    let (heads, heads_rx) = watch::channel(Some(held(7)));
    let cfg = dev_config();
    let mut state = state(&node, &dir, &cfg.api);
    let (key, vault) = (Arc::new(EnclaveKey::test_key()), cfg.custody.vault.parse().unwrap());
    let signing = Signing::new(key, cfg.custody.chain_id, vault, Duration::from_millis(50), time);
    let vault_signer = watch::channel(None).1;
    state.signing = Arc::new(signing.with_liveness(crate::liveness::Gate::default(), heads_rx, vault_signer));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let mut plain = connect(addr, &format!("signed=1&challenge={CHALLENGE}")).await;
    assert_eq!(next_text_or_end(&mut plain).await, None, "no heads asked for: the halt closes it");
    let mut ws = connect(addr, &format!("signed=1&heads=1&challenge={CHALLENGE}")).await;
    let first: serde_json::Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
    assert_eq!((first["type"].as_str(), first.get("sig")), (Some("log_head"), None), "a bare head: {first}");
    assert_eq!(first["head"]["head"]["seq"], 7);
    // Heartbeats fall due and are skipped, not closing the connection: the next head still arrives.
    tokio::time::sleep(Duration::from_millis(300)).await;
    heads.send_replace(Some(held(8)));
    let next: serde_json::Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
    assert_eq!((next["type"].as_str(), next["head"]["head"]["seq"].as_u64()), (Some("log_head"), Some(8)));
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
        "heads=1".to_owned(),
        format!("signed=1&heads=2&challenge={CHALLENGE}"),
    ] {
        let err = connect_async(format!("ws://{addr}/v1/stream?{query}")).await.err().unwrap();
        let tokio_tungstenite::tungstenite::Error::Http(response) = err else { panic!("{query}: {err}") };
        assert_eq!(response.status(), 400, "{query}");
    }
}

// ── Helpers ──

/// A held head at `seq`, signed under epoch 1.
fn held(seq: u64) -> crate::log_heads::SignedHead {
    use crate::log_heads::{Head, HeadV2, Hold};
    let head = HeadV2 {
        head: Head { seq, link: alloy::primitives::B256::repeat_byte(7) },
        parent: None,
        turn: 1,
        position: 0,
        release: alloy::primitives::B256::ZERO,
        generation: 1,
        stamp: 0,
        signed_at: 1_000,
        hold: Hold::TimeHalt,
        confirmer: alloy::primitives::Address::ZERO,
        block_number: 10,
        block_hash: alloy::primitives::B256::repeat_byte(1),
    };
    crate::log_heads::SignedHead { epoch: 1, head, signature: format!("0x{}", "11".repeat(65)) }
}

/// Serves the API for `node` on a free local port, with signed heartbeats every `heartbeat`.
async fn serve(node: &TestSequencer, dir: &std::path::Path, heartbeat: Duration) -> SocketAddr {
    serve_at(node, dir, heartbeat, trusted(Arc::new(TestClock::default()))).await
}

/// `serve`, signing at the times `time` reads.
async fn serve_at(node: &TestSequencer, dir: &std::path::Path, heartbeat: Duration, time: TrustedClock) -> SocketAddr {
    let cfg = dev_config();
    let mut state = state(node, dir, &cfg.api);
    let vault = cfg.custody.vault.parse().unwrap();
    let key = Arc::new(EnclaveKey::test_key());
    state.signing = Arc::new(Signing::new(key, cfg.custody.chain_id, vault, heartbeat, time));
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

/// The next text message, or `None` once the connection closes or ends, within ten seconds.
async fn next_text_or_end(ws: &mut Ws) -> Option<String> {
    loop {
        match timeout(Duration::from_secs(10), ws.next()).await.expect("a message or the end in time") {
            Some(Ok(WsMessage::Text(text))) => return Some(text.to_string()),
            Some(Ok(WsMessage::Close(_)) | Err(_)) | None => return None,
            Some(Ok(_)) => {}
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

/// A frame the way every connection made it before the message's parts were shared.
fn reference_frame(
    signing: &Signing,
    connection: &Connection,
    previous: &[u8; 32],
    n: u64,
    msg: &str,
) -> (String, [u8; 32]) {
    let time = signing.time_ms().unwrap();
    let (digest, sig) = signing.sign_frame(connection, previous, n, time, msg);
    let frame = SignedFrame { v: FRAME_VERSION, n, time, msg: msg.to_owned(), sig };
    (serde_json::to_string(&frame).unwrap(), digest)
}

#[tokio::test]
async fn a_frame_from_shared_parts_is_the_frame_serde_writes() {
    let signing = crate::test_support::signing();
    let connection = opened("signed=1");
    let (_, manifest) = watch::channel(Manifest::default());
    let mut framing = Framing::new(signing.clone(), connection, manifest, false);
    let big = "\u{1F412} \"quoted\" \\ \n\t\u{0}\u{1f} <&> é".repeat(1000);
    let mut previous = [0; 32];
    for (n, msg) in ["", "{}", r#"{"a":"b\"c"}"#, big.as_str()].into_iter().enumerate() {
        let (expected, digest) = reference_frame(&signing, &connection, &previous, n as u64, msg);
        let escaped = serde_json::to_string(msg).unwrap();
        let sha256: [u8; 32] = sha2::Sha256::digest(msg.as_bytes()).into();
        // Signing is deterministic, so the whole frame, signature included, matches byte for byte.
        assert_eq!(framing.frame_parts(&sha256, &escaped).unwrap(), expected, "message {n}");
        previous = digest;
    }
}

#[tokio::test]
async fn many_signed_subscribers_get_the_messages_byte_for_byte_from_one_encoding() {
    let dir = scratch_dir("signed-many");
    let node = TestSequencer::start_legacy(&dir, 1024);
    for n in 0..5 {
        node.submit(deposit(1, &format!("dep-{n}"))).await;
    }
    let addr = serve(&node, &dir, Duration::from_secs(5)).await;
    let expected = log_messages(&dir, Offset::start_of(0));
    let query = format!("from=0&signed=1&challenge={CHALLENGE}");
    let unsigned = connect(addr, "from=0").await;
    let mut subscribers = [connect(addr, &query).await, connect(addr, &query).await, connect(addr, &query).await];
    let mut unsigned = unsigned;
    for ws in subscribers.iter_mut() {
        let mut verifier = FrameVerifier::new(signer(), deployment(), opened(&query));
        let mut messages = Vec::new();
        while messages.len() < expected.len() {
            let frame: SignedFrame = serde_json::from_str(&next_text(ws).await).unwrap();
            assert_eq!(verifier.verify(&frame), Ok(()), "frame {}", frame.n);
            if !is_heartbeat(&frame) {
                messages.push(frame.msg);
            }
        }
        assert_eq!(messages, expected);
    }
    for message in &expected {
        assert_eq!(&next_text(&mut unsigned).await, message);
    }
}

#[tokio::test]
async fn a_message_is_encoded_once_however_many_read_it() {
    let (offset, entry) = {
        let dir = scratch_dir("signed-once");
        let _node = TestSequencer::start_legacy(&dir, 16);
        let message = log_messages(&dir, Offset::start_of(0)).remove(0);
        let msg: StreamMsg = serde_json::from_str(&message).unwrap();
        (msg.offset, msg.body)
    };
    let msg = StreamMsg::from_body(offset, entry);
    assert_eq!(msg.json().unwrap().as_ptr(), msg.json().unwrap().as_ptr());
    assert!(std::ptr::eq(msg.framed().unwrap(), msg.framed().unwrap()));
    assert_eq!(msg.json().unwrap(), serde_json::to_string(&msg).unwrap());
}
