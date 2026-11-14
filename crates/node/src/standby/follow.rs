//! One follow stint: the signed stream, its frames, and what they say about the copy. The
//! module doc on `Chain` says what extends a copy; this is the wire path that feeds it.

use engine_core::types::AccountId;
use std::time::Duration;

use anyhow::Context as _;
use futures_util::{StreamExt, pin_mut};
use rand::Rng;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;

use super::{FETCH_TIMEOUT, stream_url, tls};

/// What a signed heartbeat names: the operator's flushed log head, or `None` before its first entry.
#[derive(serde::Deserialize)]
struct Heartbeat {
    head: Option<Offset>,
}
use super::chain::{Chain, Step};
use super::operator::Followed;
use crate::config::NodeConfig;
use crate::log::{LogEntry, LogWriter, Offset};
use crate::sequencer::replay::LiveReplay;

/// An entry the operator sealed that the replica refused: the replica may hold part of it, so it is
/// rebuilt from the copy, which did not take the entry.
#[derive(Debug)]
pub(crate) struct ReplicaRefused;

impl std::fmt::Display for ReplicaRefused {
    /// What happened.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the replica refused an entry the operator sealed")
    }
}

impl std::error::Error for ReplicaRefused {}

/// One follow stint: opens a signed stream at the copy's last command, verifies every frame against
/// the followed enclave's address, checks each heartbeat and entry against the copy (`Chain`), and
/// applies each new entry to `replica`, if any, before appending it, for up to `budget`. Returns the
/// copy's head. A closed, failed, or ended connection returns to re-check the chain; the next stint
/// opens a fresh connection, which the verifier binds, so a spliced connection is never trusted
/// across stints.
/// Error cases:
/// - the connection fails, a frame fails verification, or the copy cannot be written
/// - the operator's log no longer extends the copy (a `Rollback`); nothing more is appended
/// - the replica refuses an entry (`ReplicaRefused`), which the copy then does not take
pub(super) async fn follow(
    writer: &mut LogWriter,
    copied: &mut Chain,
    cfg: &NodeConfig,
    followed: &Followed,
    budget: Duration,
    mut replica: Option<&mut LiveReplay>,
) -> anyhow::Result<Option<Offset>> {
    let mut challenge = [0u8; 32];
    rand::thread_rng().fill(&mut challenge);
    let path_and_query =
        format!("/v1/stream?signed=1&challenge={}&from={}", hex::encode(challenge), copied.resume_seq());
    let url = stream_url(&followed.origin, &path_and_query)?;
    let connector = tokio_tungstenite::Connector::Rustls(tls(followed.danger_accept_invalid_certs)?);
    let connect = tokio_tungstenite::connect_async_tls_with_config(url.as_str(), None, false, Some(connector));
    let (mut stream, _) = timeout(FETCH_TIMEOUT, connect)
        .await
        .context("connecting to the stream timed out")?
        .with_context(|| format!("connecting to {url}"))?;
    let mut verifier = engine_wire::FrameVerifier::new(
        followed.enclave.0,
        engine_wire::Deployment {
            chain_id: cfg.custody.chain_id,
            vault: cfg.custody.vault.parse::<AccountId>().context("custody.vault")?.0,
        },
        engine_wire::Connection::new(challenge, &path_and_query),
    );
    let deadline = sleep(budget);
    pin_mut!(deadline);
    copied.begin();
    // Every exit path flushes: a later stint resumes the stream from the copy's last command, so
    // an unflushed append would be skipped forever.
    let followed = async {
        // A connection the host holds open but feeds nothing must not read as a working stint: no
        // frame, no evidence of anything.
        let mut frames = 0usize;
        loop {
            let frame = {
                let next = stream.next();
                futures_util::pin_mut!(next);
                match futures_util::future::select(&mut deadline, next).await {
                    futures_util::future::Either::Left(_) => break,
                    futures_util::future::Either::Right((message, _)) => {
                        let message = message.context("reading the stream")??;
                        let text = match message {
                            Message::Text(text) => text,
                            Message::Close(_) => break,
                            // The stream sends text frames; anything else is ignored, as the
                            // subscriber side of the API does with what a subscriber sends.
                            _ => continue,
                        };
                        verifier.verify_text(&text).context("a stream frame failed verification")?
                    }
                }
            };
            frames += 1;
            let value: serde_json::Value = serde_json::from_str(&frame.msg).context("a stream message is not JSON")?;
            if value.get("type").and_then(serde_json::Value::as_str) == Some("heartbeat") {
                let heartbeat: Heartbeat = serde_json::from_value(value).context("a heartbeat names no log head")?;
                copied.heartbeat(heartbeat.head)?;
                continue;
            }
            let message: crate::sequencer::StreamMsg =
                serde_json::from_value(value).context("a stream message is not a log entry")?;
            let entry = LogEntry::from(message.body);
            if let Step::Extends(link) = copied.check(message.offset, &entry)? {
                if let Some(replica) = replica.as_deref_mut() {
                    replica.apply(&entry).map_err(|e| e.context(ReplicaRefused))?;
                }
                writer.append(&entry).context("appending a stream entry to the copy")?;
                copied.extend(message.offset, entry, link);
                writer.flush().context("flushing the copy")?;
            }
        }
        anyhow::ensure!(frames > 0, "the stream delivered no frame in the stint's {budget:?}");
        anyhow::Ok(copied.head())
    }
    .await;
    writer.sync().context("syncing the copy")?;
    followed
}
