//! What carries a venue's messages: a WebSocket over TLS.

use std::pin::Pin;
use std::sync::Arc;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use rustls::ClientConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};

use crate::config::VenueConnection;

/// One message read from a venue.
#[derive(Debug)]
pub(super) enum Incoming {
    Text(String),
    Binary,
    /// The venue closed the connection, for the reason given.
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
/// `wss://` (see `VenueConnection::validate_endpoint`).
/// Error cases:
/// - the URL does not parse
/// - the connection or the WebSocket handshake fails
pub(super) async fn connect(conn: &VenueConnection, tls: Arc<ClientConfig>) -> anyhow::Result<Link> {
    let (ws, _) = connect_async_tls_with_config(conn.url.as_str(), None, false, Some(Connector::Rustls(tls))).await?;
    let (sink, stream) = ws.split();
    let stream = stream.map(|message| {
        Ok(match message? {
            Message::Text(text) => Incoming::Text(text.as_str().to_owned()),
            Message::Binary(_) => Incoming::Binary,
            Message::Close(frame) => Incoming::Closed(format!("{frame:?}")),
            Message::Pong(payload) => Incoming::Pong(payload.to_vec()),
            Message::Ping(_) | Message::Frame(_) => Incoming::Other,
        })
    });
    Ok(Link { stream: Box::pin(stream), sink: Box::pin(sink.sink_map_err(anyhow::Error::from)) })
}
