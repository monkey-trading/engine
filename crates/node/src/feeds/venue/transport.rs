//! What carries a venue's messages: a WebSocket over TLS.

use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use rustls::ClientConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};

use crate::config::VenueConnection;

/// One message read from a venue.
#[derive(Debug)]
pub(super) enum Incoming {
    Text(String),
    Binary,
    /// The venue closed the connection, with its close frame's code (see `closing`).
    Closed(String),
    /// The venue's answer to a ping, with that ping's payload.
    Pong(Vec<u8>),
    /// A ping or raw frame: nothing to read.
    Other,
}

/// One open connection to a venue: where its messages arrive and where ours go.
pub(super) struct Link {
    pub stream: Pin<Box<dyn Stream<Item = anyhow::Result<Incoming>> + Send>>,
    sink: Pin<Box<dyn Sink<Message, Error = anyhow::Error> + Send>>,
}

impl Link {
    /// Sends `messages` in order.
    /// Error cases:
    /// - the connection fails while sending
    pub(super) async fn send_all(&mut self, messages: Vec<String>) -> anyhow::Result<()> {
        for message in messages {
            self.sink.send(Message::text(message)).await?;
        }
        Ok(())
    }

    /// Sends a WebSocket ping carrying `payload`.
    /// Error cases:
    /// - the connection fails while sending
    pub(super) async fn ping(&mut self, payload: Vec<u8>) -> anyhow::Result<()> {
        self.sink.send(Message::Ping(payload.into())).await
    }
}

/// Connects to `conn`'s endpoint over a WebSocket with `tls`, which the config requires to be
/// `wss://` (see `VenueConnection::validate_endpoint`), the handshake carrying `bearer` as its
/// `Authorization` where the venue takes one.
/// Error cases:
/// - the URL does not parse, or `bearer` is not a header value
/// - the connection or the WebSocket handshake fails
pub(super) async fn connect(
    conn: &VenueConnection,
    bearer: Option<&str>,
    tls: Arc<ClientConfig>,
) -> anyhow::Result<Link> {
    let mut request = conn.url.as_str().into_client_request()?;
    if let Some(token) = bearer {
        // The error names no part of the value.
        let mut value = HeaderValue::from_str(&format!("Bearer {token}")).context("bearer token")?;
        value.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, value);
    }
    let (ws, _) = connect_async_tls_with_config(request, None, false, Some(Connector::Rustls(tls))).await?;
    let (sink, stream) = ws.split();
    let stream = stream.map(|message| {
        Ok(match message? {
            Message::Text(text) => Incoming::Text(text.as_str().to_owned()),
            Message::Binary(_) => Incoming::Binary,
            Message::Close(frame) => Incoming::Closed(
                frame.map_or_else(|| "no close frame".to_owned(), |f| closing(f.code.into(), &f.reason)),
            ),
            Message::Pong(payload) => Incoming::Pong(payload.to_vec()),
            Message::Ping(_) | Message::Frame(_) => Incoming::Other,
        })
    });
    Ok(Link { stream: Box::pin(stream), sink: Box::pin(sink.sink_map_err(anyhow::Error::from)) })
}

/// A close frame for a log line: its code, and only the length of its reason, text the venue wrote,
/// which could carry a credential or token in some encoding.
fn closing(code: u16, reason: &str) -> String {
    format!("code {code}, a {}-byte reason", reason.len())
}

#[cfg(test)]
mod tests;
