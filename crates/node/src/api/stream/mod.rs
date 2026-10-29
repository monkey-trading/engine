//! `/v1/stream`: the public log over a WebSocket. A subscriber that names an offset gets the log
//! from there, read from disk, then live messages, each once and in offset order. A subscriber
//! that falls behind the live channel reads from disk again from its last offset instead of being
//! dropped. A subscriber that stops reading is closed, and can resume. Subscribers are capped by
//! `api.max_subscribers`, and what a subscriber sends is read and dropped, so its close is
//! answered. A subscriber that asks for `signed=1` gets each message in a signed frame, with
//! heartbeats (`signed`); without it, the messages go out as they are.

use std::collections::VecDeque;
use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use axum::Error;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::Uri;
use axum::response::Response;
use engine_wire::Connection;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, watch};
use tokio::task::coop::consume_budget;
use tokio::task::{JoinHandle, spawn_blocking};
use tokio::time::timeout;
use tracing::field::display;
use tracing::{Instrument, Span, error, info};

use crate::api::{ApiError, ApiState};
use crate::log::{LogReader, Manifest, Offset};
use crate::sequencer::StreamMsg;
mod signed;
use signed::Framing;

/// Most messages read from disk per blocking call.
const BACKFILL_BATCH: usize = 256;
/// Subscribers reading a batch from disk at once, each holding one checked chunk of a segment while
/// it reads. A subscriber keeps up to `BACKFILL_BATCH` decoded messages after it reads them.
pub const MAX_BACKFILLS: usize = 16;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamParams {
    /// Start right after this offset: the last one the subscriber received.
    after: Option<Offset>,
    /// Start at the first message of this seq; `0` is the whole log.
    from: Option<u64>,
    /// `1` sends each message in a frame the enclave key signs, with a heartbeat at least every
    /// `api.heartbeat_interval`.
    signed: Option<u8>,
    /// With `signed=1`, and only with it, the client's fresh 64 hex characters, which every frame
    /// is signed under.
    challenge: Option<String>,
}

/// One subscriber's messages, in offset order, each once.
pub(crate) struct Feed {
    live: broadcast::Receiver<Arc<StreamMsg>>,
    log_dir: PathBuf,
    manifest: watch::Receiver<Manifest>,
    /// Shared by every subscriber; one is held while reading a batch from disk.
    backfills: Arc<Semaphore>,
    /// The lowest offset not yet yielded; `None` until a live-only feed yields its first message.
    next: Option<Offset>,
    /// Reading authenticated completed descriptors or the writer-committed active prefix.
    /// The compatible Manifest watch carries a lazy catalog, never a hydrated historical Vec.
    /// Reading from disk until it reaches what has been written.
    catching_up: bool,
    reader: Option<LogReader>,
    /// Read from disk, not yet yielded.
    backlog: VecDeque<Arc<StreamMsg>>,
}

impl Feed {
    /// A feed that starts at `start`, or with the next live message if `None`. `live` must be
    /// subscribed before the feed first reads the disk, so that every message is either on disk by
    /// then or still to come live.
    pub(crate) fn new(
        live: broadcast::Receiver<Arc<StreamMsg>>,
        log_dir: PathBuf,
        manifest: watch::Receiver<Manifest>,
        backfills: Arc<Semaphore>,
        start: Option<Offset>,
    ) -> Self {
        let (catching_up, reader, backlog) = (start.is_some(), None, VecDeque::new());
        Self { live, log_dir, manifest, backfills, next: start, catching_up, reader, backlog }
    }

    /// The next message, or `None` once the sequencer has stopped.
    /// Error cases:
    /// - the log on disk cannot be read (see `LogReader`)
    /// - the next live message does not follow the last one yielded, so history is missing from
    ///   the log on disk
    pub(crate) async fn next(&mut self) -> anyhow::Result<Option<Arc<StreamMsg>>> {
        loop {
            if let Some(msg) = self.backlog.pop_front() {
                self.next = Some(msg.offset.next());
                return Ok(Some(msg));
            }
            if self.catching_up {
                // Catching up starts only from a start offset, or after a lag once one is set.
                let from = self.next.expect("a feed catches up from a known offset");
                let permit: OwnedSemaphorePermit =
                    self.backfills.clone().acquire_owned().await.expect("the backfill semaphore is never closed");
                let (dir, manifest, reader) = (self.log_dir.clone(), self.manifest.clone(), self.reader.take());
                let (reader, batch) = read_holding(permit, move || read_batch(&dir, manifest, from, reader))
                    .await
                    .context("log reader task")??;
                self.catching_up = reader.is_some();
                self.reader = reader;
                self.backlog.extend(batch);
                continue;
            }
            match self.live.recv().await {
                // Before the next offset to yield: already yielded, or before the start.
                Ok(msg) if self.next.is_some_and(|next| msg.offset < next) => {}
                Ok(msg) => {
                    if let Some(next) = self.next {
                        ensure!(
                            // The next seq follows only once this one has yielded its command.
                            msg.offset == next || (next.index > 0 && msg.offset == Offset::start_of(next.seq + 1)),
                            "messages from {next} up to {} are missing from the log on disk",
                            msg.offset
                        );
                    }
                    self.next = Some(msg.offset.next());
                    return Ok(Some(msg));
                }
                // What the channel dropped is on disk. A live-only feed that has yielded nothing yet
                // has no gap: it starts at whatever the channel still holds.
                Err(RecvError::Lagged(_)) => self.catching_up = self.next.is_some(),
                Err(RecvError::Closed) => return Ok(None),
            }
        }
    }
}

/// Upgrades to a WebSocket that streams the public log: live from now by default, or from `after`
/// or `from` through what is on disk and then live, in signed frames with `signed=1`. The
/// subscriber holds one of the capped places until it disconnects, and may send messages up to
/// `api.max_body_bytes`.
/// Error cases:
/// - both `after` and `from` are given
/// - `signed` is not `1`, it comes without a `challenge` or a `challenge` without it, or the
///   challenge is not 64 hex characters
/// - every subscriber place is taken
pub async fn stream(
    State(s): State<ApiState>,
    Query(params): Query<StreamParams>,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let start = match (params.after, params.from) {
        (Some(_), Some(_)) => return Err(ApiError::BadRequest("give after or from, not both".into())),
        (Some(after), None) => Some(after.next()),
        (None, Some(seq)) => Some(Offset::start_of(seq)),
        (None, None) => None,
    };
    let framing = match (params.signed, params.challenge.as_deref()) {
        (None, None) => None,
        (Some(1), Some(challenge)) => {
            let challenge = engine_wire::parse_challenge(challenge)
                .ok_or_else(|| ApiError::BadRequest("challenge must be 64 hex characters".into()))?;
            // Frames bind the request exactly as the client sent it, start included.
            let request = uri.path_and_query().map_or(uri.path(), |request| request.as_str());
            let connection = Connection::new(challenge, request);
            Some(Framing::new(s.signing.clone(), connection, s.clock, s.manifest.clone()))
        }
        (Some(1), None) => return Err(ApiError::BadRequest("signed=1 needs a challenge".into())),
        (None, Some(_)) => return Err(ApiError::BadRequest("a challenge needs signed=1".into())),
        (Some(_), _) => return Err(ApiError::BadRequest("signed must be 1".into())),
    };
    Span::current().record("id", display(start.map_or_else(|| "live".to_owned(), |from| from.to_string())));
    let place = s.limits.subscribe()?;
    let feed = Feed::new(s.stream.subscribe(), s.log_dir.clone(), s.manifest.clone(), s.backfills.clone(), start);
    let (send_timeout, span) = (s.limits.send_timeout, Span::current());
    let ws = ws.max_message_size(s.limits.max_body_bytes).max_frame_size(s.limits.max_body_bytes);
    Ok(ws.on_upgrade(move |socket| {
        async move {
            forward(socket, feed, send_timeout, framing).await;
            drop(place);
        }
        .instrument(span)
    }))
}

/// Sends the feed to one subscriber until it closes or disconnects, stops reading for
/// `send_timeout`, or the sequencer stops. What the subscriber sends is read alongside, so its
/// close is answered while messages are still waiting for it.
async fn forward<S>(socket: S, feed: Feed, send_timeout: Duration, framing: Option<Framing>)
where
    S: Sink<Message> + Stream<Item = Result<Message, Error>> + Unpin,
{
    let (mut sink, mut input) = socket.split();
    tokio::select! {
        closed = read_until_closed(&mut input) => if closed {
            // Sends the close reply queued when the close was read.
            let _ = timeout(send_timeout, sink.close()).await;
        },
        () = send_feed(&mut sink, feed, send_timeout, framing) => {}
    }
}

/// Reads and drops what the subscriber sends, which also answers its pings, until it closes
/// (`true`), or fails or disconnects (`false`).
async fn read_until_closed<S: Stream<Item = Result<Message, Error>> + Unpin>(input: &mut S) -> bool {
    while let Some(Ok(msg)) = input.next().await {
        if let Message::Close(_) = msg {
            return true;
        }
        // A subscriber that floods its input yields to its own stream, so it is still sent to.
        consume_budget().await;
    }
    false
}

/// Sends the feed until the subscriber disconnects, stops reading for `send_timeout`, or the
/// sequencer stops; with `framing`, in signed frames between heartbeats.
async fn send_feed<S: Sink<Message> + Unpin>(
    sink: &mut S,
    mut feed: Feed,
    send_timeout: Duration,
    mut framing: Option<Framing>,
) {
    loop {
        let next = feed.next();
        tokio::pin!(next);
        // The feed's read stays in flight while heartbeats go out, so a slow backfill still ends.
        let msg = loop {
            tokio::select! {
                msg = &mut next => break msg,
                heartbeat = next_heartbeat(&mut framing) => {
                    if !send_text(sink, heartbeat, send_timeout, None).await {
                        return;
                    }
                }
            }
        };
        let msg = match msg {
            Ok(Some(msg)) => msg,
            Ok(None) => return,
            Err(e) => {
                error!(alert = "stream_backfill_failed", "{e:#}");
                return;
            }
        };
        // Closing rather than skipping keeps the subscriber's view gap-free; it can resume.
        let json = match serde_json::to_string(&*msg) {
            Ok(json) => json,
            Err(e) => {
                error!(alert = "stream_encode_failed", "encoding the message at {}: {e}", msg.offset);
                return;
            }
        };
        let text = match framing.as_mut() {
            Some(framing) => framing.frame(json),
            None => json,
        };
        if !send_text(sink, text, send_timeout, Some(msg.offset)).await {
            return;
        }
    }
}

/// The connection's next heartbeat frame once it is due; never, on an unsigned connection.
async fn next_heartbeat(framing: &mut Option<Framing>) -> String {
    match framing {
        Some(framing) => framing.next_heartbeat().await,
        None => pending().await,
    }
}

/// Sends `text`, the message at `offset` or a heartbeat; `false` once the subscriber has
/// disconnected or stopped reading for `send_timeout`.
async fn send_text<S: Sink<Message> + Unpin>(
    sink: &mut S,
    text: String,
    send_timeout: Duration,
    offset: Option<Offset>,
) -> bool {
    match timeout(send_timeout, sink.send(Message::Text(text.into()))).await {
        Ok(Ok(())) => true,
        Ok(Err(_)) => false,
        Err(_) => {
            info!(offset = offset.map(display), "closing a subscriber that stopped reading");
            false
        }
    }
}

/// Runs `read` on the blocking pool holding `permit` until it returns, so a subscriber that leaves
/// mid-read does not free its backfill permit while the read still runs.
fn read_holding<T: Send + 'static>(
    permit: OwnedSemaphorePermit,
    read: impl FnOnce() -> T + Send + 'static,
) -> JoinHandle<T> {
    spawn_blocking(move || {
        let _held = permit;
        read()
    })
}

/// Up to `BACKFILL_BATCH` messages from disk, opening a reader at `from` when there is none yet,
/// and the reader to go on with, released so it holds no segment in memory, or `None` once it has
/// read everything written.
/// Error cases:
/// - the log cannot be read (see `LogReader`)
fn read_batch(
    dir: &Path,
    manifest: watch::Receiver<Manifest>,
    from: Offset,
    reader: Option<LogReader>,
) -> anyhow::Result<(Option<LogReader>, Vec<Arc<StreamMsg>>)> {
    let mut reader = match reader {
        Some(reader) => reader,
        None => LogReader::open(dir, from, manifest)?,
    };
    let mut batch = Vec::new();
    while batch.len() < BACKFILL_BATCH {
        let Some((offset, entry)) = reader.next_entry()? else {
            return Ok((None, batch));
        };
        batch.push(Arc::new(StreamMsg::new(offset, entry)));
    }
    reader.release();
    Ok((Some(reader), batch))
}

#[cfg(test)]
mod tests;
