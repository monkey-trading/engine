use super::*;

use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{CloseCode, Data, OpCode};

use crate::config::Venue;
use crate::feeds::tests::rwa_support::TlsFront;

#[tokio::test]
async fn a_close_frame_is_read_by_its_code_and_the_length_of_its_reason() {
    // A reason echoing a token, as it is, JSON-escaped, and in a reason that is not JSON.
    let reasons = [r#"{"token":"tb\/4K+v"}"#, "token tb/4K+v"];
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let url = format!("ws://{}/", listener.local_addr().expect("an address"));
    let venue = tokio::spawn(async move {
        for reason in reasons {
            let (tcp, _) = listener.accept().await.expect("a connection");
            let mut ws = accept_async(tcp).await.expect("a handshake");
            ws.close(Some(CloseFrame { code: CloseCode::Policy, reason: reason.into() })).await.expect("closed");
        }
    });
    let conn = connection(url);
    let tls = TlsFront::new();
    for reason in reasons {
        let mut link = connect(&conn, None, tls.client.clone()).await.expect("connected");
        let Some(Ok(Incoming::Closed(closed))) = link.stream.next().await else { panic!("a close frame") };
        assert_eq!(closed, format!("code 1008, a {}-byte reason", reason.len()));
    }
    venue.await.expect("the venue");
}

#[tokio::test]
async fn a_message_longer_than_the_bound_fails_the_connection_though_each_frame_is_shorter() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let url = format!("ws://{}/", listener.local_addr().expect("an address"));
    let half = MAX_MESSAGE_BYTES / 2;
    let venue = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("a connection");
        let mut ws = accept_async(tcp).await.expect("a handshake");
        // Each message in two frames: one at the bound, then one a byte past it.
        for last in [half, half + 1] {
            let first = Frame::message("x".repeat(half), OpCode::Data(Data::Text), false);
            let rest = Frame::message("x".repeat(last), OpCode::Data(Data::Continue), true);
            // The connection may already be failed by the second message.
            let _ = ws.send(Message::Frame(first)).await;
            let _ = ws.send(Message::Frame(rest)).await;
        }
        let _ = ws.next().await;
    });
    let tls = TlsFront::new();
    let mut link = connect(&connection(url), None, tls.client.clone()).await.expect("connected");
    let Some(Ok(Incoming::Text(text))) = link.stream.next().await else { panic!("the message at the bound") };
    assert_eq!(text.len(), MAX_MESSAGE_BYTES);
    let refused = link.stream.next().await.expect("a second message").expect_err("refused");
    assert!(format!("{refused:#}").contains("Message too long"), "{refused:#}");
    drop(link);
    venue.await.expect("the venue");
}

// ── Helpers ──

/// A connection to `url` with no credentials.
fn connection(url: String) -> VenueConnection {
    VenueConnection {
        venue: Venue::Cme,
        url,
        api_key_env: None,
        api_key_secret: None,
        oauth: None,
        key_pair: None,
        warm: None,
        symbols: Vec::new(),
    }
}
