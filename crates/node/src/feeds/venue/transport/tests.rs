use super::*;

use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

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
    let conn = VenueConnection {
        venue: Venue::Cme,
        url,
        api_key_env: None,
        api_key_secret: None,
        oauth: None,
        symbols: Vec::new(),
    };
    let tls = TlsFront::new();
    for reason in reasons {
        let mut link = connect(&conn, None, tls.client.clone()).await.expect("connected");
        let Some(Ok(Incoming::Closed(closed))) = link.stream.next().await else { panic!("a close frame") };
        assert_eq!(closed, format!("code 1008, a {}-byte reason", reason.len()));
    }
    venue.await.expect("the venue");
}
