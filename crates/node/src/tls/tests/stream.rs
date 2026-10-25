//! The stream end to end over TLS: the subscriber cap and the cap on what a subscriber sends.

use super::*;

use engine_core::command::{Command, Sequenced};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};

use crate::log::{CommandEntry, LogEntry, Offset};

#[tokio::test]
async fn stream_subscribers_are_capped_until_one_leaves() {
    let mut api = api();
    api.max_subscribers = NonZeroUsize::new(1).unwrap();
    let node = TestNode::start(api, V4).await;
    let (mut first, _) = client_async("wss://engine/v1/stream", node.connect(V4).await.unwrap()).await.unwrap();
    node.stream.send(tick(0)).unwrap();
    let delivered = first.next().await.unwrap().unwrap();
    assert!(delivered.to_text().unwrap().contains(r#""offset":"0:0""#), "{delivered}");

    let refused = client_async("wss://engine/v1/stream", node.connect(V4).await.unwrap()).await;
    let Err(WsError::Http(response)) = refused else { panic!("a second subscriber was not refused") };
    assert_eq!(response.status(), 503);

    // The place frees once the server reads that the first subscriber has gone.
    drop(first);
    for _ in 0..500 {
        if client_async("wss://engine/v1/stream", node.connect(V4).await.unwrap()).await.is_ok() {
            return;
        }
    }
    panic!("the first subscriber's place was never freed");
}

#[tokio::test]
async fn a_subscriber_message_at_the_cap_is_read_and_its_stream_stays_open() {
    let node = TestNode::start(api(), V4).await;
    let mut ws = subscribe(&node).await;
    ws.send(WsMessage::Text("x".repeat(1024).into())).await.unwrap();
    // A pong comes back only once the server has read past the capped message.
    ws.send(WsMessage::Ping(vec![7].into())).await.unwrap();
    let reply = timeout(Duration::from_secs(5), ws.next()).await.expect("a reply").unwrap().unwrap();
    assert_eq!(reply, WsMessage::Pong(vec![7].into()));
}

#[tokio::test]
async fn a_subscriber_frame_over_the_cap_ends_its_stream_and_frees_its_place() {
    let mut api = api();
    api.max_subscribers = NonZeroUsize::new(1).unwrap();
    let node = TestNode::start(api, V4).await;
    let mut ws = subscribe(&node).await;
    ws.send(WsMessage::Text("x".repeat(1025).into())).await.unwrap();
    assert_stream_ends(&mut ws).await;
    node.subscribe_eventually().await;
}

#[tokio::test]
async fn a_subscriber_message_over_the_cap_in_small_frames_ends_its_stream() {
    let mut api = api();
    api.max_subscribers = NonZeroUsize::new(1).unwrap();
    let node = TestNode::start(api, V4).await;
    let mut ws = subscribe(&node).await;
    let first = Frame::message(vec![b'x'; 600], OpCode::Data(Data::Text), false);
    let last = Frame::message(vec![b'x'; 600], OpCode::Data(Data::Continue), true);
    ws.send(WsMessage::Frame(first)).await.unwrap();
    ws.send(WsMessage::Frame(last)).await.unwrap();
    assert_stream_ends(&mut ws).await;
    node.subscribe_eventually().await;
}

// ── Helpers ──

impl TestNode {
    /// Subscribes to the stream once the server has freed a subscriber place, trying a bounded
    /// number of times.
    async fn subscribe_eventually(&self) {
        for _ in 0..500 {
            if client_async("wss://engine/v1/stream", self.connect(V4).await.unwrap()).await.is_ok() {
                return;
            }
        }
        panic!("no subscriber place was freed");
    }
}

/// A stream subscription over a pinned TLS connection from the IPv4 loopback.
async fn subscribe(node: &TestNode) -> WebSocketStream<ClientTls<TcpStream>> {
    client_async("wss://engine/v1/stream", node.connect(V4).await.unwrap()).await.unwrap().0
}

/// Waits for the server to end `ws`: a close frame, an error, or the end of the stream.
async fn assert_stream_ends(ws: &mut WebSocketStream<ClientTls<TcpStream>>) {
    let ended = timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            }
        }
    });
    ended.await.expect("the server ended the stream");
}

/// A tick command at `seq`, as the stream sends it.
fn tick(seq: u64) -> Arc<StreamMsg> {
    let tick =
        CommandEntry { seal: None, sequenced: Sequenced { seq, ts: 0, cmd: Command::Tick }, events: Some(Vec::new()) };
    Arc::new(StreamMsg::new(Offset::start_of(seq), LogEntry::Command(tick)))
}
