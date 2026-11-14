//! The predecessor's checks on a request for the keys before it stops: every successor must read the
//! log's protocol version, and only a tip on this log gets the tail. The sequencer answers both while
//! it runs; once it has stopped, what it answered then does, and a later tip is found on the final
//! log by extending the last tip found with the commands the log holds after it.

use anyhow::{Context, anyhow, ensure};
use engine_core::eip712::B256;
use engine_core::protocol::ProtocolVersion;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::checkpoints::CheckpointChain;
use crate::handover::Predecessor;
use crate::handover::tail::{TailMiss, Tip, ensure_reads};
use crate::log::{LogEntry, Manifest};
use crate::sequencer::Request;

/// What answers a request for the keys once the sequencer is gone: the log's protocol version, and
/// the last tip found on the log with its cumulative prefix.
#[derive(Default)]
pub(super) struct Evidence {
    version: Option<ProtocolVersion>,
    found: Option<(u64, B256)>,
}

impl Evidence {
    /// The evidence of a stopped log that runs `version`.
    #[cfg(test)]
    pub(super) fn running(version: ProtocolVersion) -> Self {
        Self { version: Some(version), found: None }
    }
}

impl<C: CheckpointChain> Predecessor<C> {
    /// Refuses a successor whose build reads only protocol versions older than the log runs, or that
    /// names none (`reads`), since it could not replay the log it takes.
    /// Error cases:
    /// - the log runs a protocol version newer than `reads`, `reads` names no version, or the log's
    ///   version cannot be read
    pub(super) async fn ensure_successor_reads(&self, reads: &str) -> anyhow::Result<()> {
        let version = match self.query(|engine| engine.protocol_version()).await {
            Ok(version) => version,
            Err(e) => self.tail_evidence.lock().expect("the tail evidence lock").version.ok_or(e)?,
        };
        self.tail_evidence.lock().expect("the tail evidence lock").version = Some(version);
        ensure_reads(version, reads)
    }

    /// The seq the tail follows for a successor at `tip`, once its link is the cumulative prefix this
    /// log holds after that seq; otherwise why not, which the predecessor refuses with before its stop.
    /// Error cases:
    /// - the tip names another link, or a seq past the log's head (`NotOnLog`), or one older than the
    ///   log retains, or one this listener cannot find once its sequencer is gone (`Lagged`)
    pub(super) async fn tail_after(&self, tip: &Tip) -> Result<u64, TailMiss> {
        let (held, head) = match self.prefix_at(tip.seq).await {
            Ok(held) => (held, self.query(|engine| engine.last_seq()).await.ok().flatten()),
            Err(_) => {
                let held = self.prefix_on_disk(tip.seq).unwrap_or_else(|e| {
                    warn!("finding the successor's tip on the stopped log failed: {e:#}");
                    None
                });
                (held, None)
            }
        };
        match held {
            Some(link) if link == tip.link => {}
            Some(link) => {
                warn!(tip_seq = tip.seq, tip_link = %tip.link, log_link = %link, "the successor's tip is not on this log");
                return Err(TailMiss::NotOnLog);
            }
            None if head.is_some_and(|head| tip.seq > head) => {
                warn!(tip_seq = tip.seq, head_seq = ?head, "the successor's tip is past this log's head");
                return Err(TailMiss::NotOnLog);
            }
            None => {
                warn!(tip_seq = tip.seq, "the successor's tip is older than this log retains");
                return Err(TailMiss::Lagged);
            }
        }
        info!(tip_seq = tip.seq, "the successor's tip is on this log: only the tail past it goes");
        self.tail_evidence.lock().expect("the tail evidence lock").found = Some((tip.seq, tip.link));
        Ok(tip.seq)
    }

    /// Retains the log's protocol version as sequencing stops, which answers a request for the keys
    /// once the sequencer is gone; a read that fails retains none, which refuses such a request.
    pub(super) async fn retain_version(&self) {
        let version = self.query(|engine| engine.protocol_version()).await.ok();
        let mut evidence = self.tail_evidence.lock().expect("the tail evidence lock");
        evidence.version = evidence.version.or(version);
    }

    /// The cumulative prefix the sequencer retains after the command at `seq`.
    /// Error cases:
    /// - the listener reads no log, or the node is stopping
    async fn prefix_at(&self, seq: u64) -> anyhow::Result<Option<B256>> {
        let engine = self.engine.as_ref().context("this listener reads no log to find the tip on")?;
        let engine = engine.upgrade().ok_or_else(|| anyhow!("the node is stopping"))?;
        let (reply, answered) = oneshot::channel();
        engine.send(Request::PrefixAt { seq, reply }).await.map_err(|_| anyhow!("the node is stopping"))?;
        answered.await.map_err(|_| anyhow!("the node is stopping"))
    }

    /// The cumulative prefix after the command at `seq` on the stopped log, extended from the last tip
    /// found on it with the commands the log holds after that one; `None` without one at or before
    /// `seq`.
    /// Error cases:
    /// - the manifest or a segment cannot be read, or a segment differs from its listing
    fn prefix_on_disk(&self, seq: u64) -> anyhow::Result<Option<B256>> {
        let found = self.tail_evidence.lock().expect("the tail evidence lock").found;
        let Some((mut at, mut prefix)) = found.filter(|(found, _)| *found <= seq) else { return Ok(None) };
        let manifest = Manifest::load(&self.log_dir)?;
        let from = at;
        for listed in manifest.segments.iter().filter(|listed| listed.last_seq > from && listed.first_seq <= seq) {
            for line in self.segment_bytes(listed)?.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
                if let LogEntry::Command(command) =
                    serde_json::from_slice(line).context("a log line is not an entry")?
                    && command.sequenced.seq > at
                    && command.sequenced.seq <= seq
                {
                    ensure!(
                        command.sequenced.seq == at + 1,
                        "the log skips from seq {at} to {}",
                        command.sequenced.seq
                    );
                    prefix =
                        B256::from(crate::log::extend_command_prefix(prefix.0, crate::log::command_digest(&command)?));
                    at = command.sequenced.seq;
                }
            }
        }
        Ok((at == seq).then_some(prefix))
    }
}
