//! What carries a venue's messages: a WebSocket over TLS, or, for Databento's live gateway, lines
//! of text over plain TCP.

use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use rustls::ClientConfig;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};
use tokio_util::codec::{Framed, LinesCodec};

use crate::config::VenueConnection;

/// The longest line Databento's gateway may send: its control messages are capped at 64 KiB, and a
/// record is far shorter. A longer line ends the connection rather than growing a buffer.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// One message read from a venue.
#[derive(Debug)]
pub(super) enum Incoming {
    Text(String),
    Binary,
    /// The venue closed the connection, for the reason given.
    Closed(String),
    /// A ping, pong, or raw frame: nothing to read.
    Other,
}

/// One open connection to a venue: where its messages arrive and where ours go.
pub(super) struct Link {
    pub stream: Pin<Box<dyn Stream<Item = anyhow::Result<Incoming>> + Send>>,
    sink: Pin<Box<dyn Sink<String, Error = anyhow::Error> + Send>>,
}

impl Link {
    /// Sends `messages` in order.
    /// Error cases:
    /// - the connection fails while sending
    pub(super) async fn send_all(&mut self, messages: Vec<String>) -> anyhow::Result<()> {
        for message in messages {
            self.sink.send(message).await?;
        }
        Ok(())
    }
}

/// Connects to `conn`'s endpoint: a `tcp://` URL over plain TCP in lines, any other over a
/// WebSocket with `tls`.
/// Error cases:
/// - the URL does not parse, or names no host or port for TCP
/// - the connection or the WebSocket handshake fails
pub(super) async fn connect(conn: &VenueConnection, tls: Arc<ClientConfig>) -> anyhow::Result<Link> {
    let uri: Uri = conn.url.parse().context("url")?;
    if uri.scheme_str() == Some("tcp") {
        let host = uri.host().ok_or_else(|| anyhow!("url has no host"))?;
        let port = uri.port_u16().ok_or_else(|| anyhow!("url has no port"))?;
        let (sink, stream) =
            Framed::new(TcpStream::connect((host, port)).await?, LinesCodec::new_with_max_length(MAX_LINE_BYTES))
                .split();
        let stream = stream.map(|line| line.map(Incoming::Text).context("read line"));
        let sink = sink.sink_map_err(|e| anyhow!("send line: {e}"));
        return Ok(Link { stream: Box::pin(stream), sink: Box::pin(sink) });
    }
    let (ws, _) = connect_async_tls_with_config(conn.url.as_str(), None, false, Some(Connector::Rustls(tls))).await?;
    let (sink, stream) = ws.split();
    let stream = stream.map(|message| {
        Ok(match message? {
            Message::Text(text) => Incoming::Text(text.as_str().to_owned()),
            Message::Binary(_) => Incoming::Binary,
            Message::Close(frame) => Incoming::Closed(format!("{frame:?}")),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Incoming::Other,
        })
    });
    let sink = sink.with(|text: String| async move { anyhow::Ok(Message::text(text)) });
    Ok(Link { stream: Box::pin(stream), sink: Box::pin(sink) })
}
