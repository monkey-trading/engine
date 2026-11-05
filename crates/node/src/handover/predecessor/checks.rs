//! The predecessor's checks before it stops: whether the tail fits the handover, and whether its
//! own log accepted the daily turn a successor takes.

use anyhow::{Context, anyhow, ensure};
use tokio::sync::oneshot;

use crate::checkpoints::{CheckpointChain, ScheduledTurn};
use crate::handover::Predecessor;
use crate::log::Manifest;
use crate::sequencer::Request;

impl<C: CheckpointChain> Predecessor<C> {
    /// Whether the segments past `tip` can be handed over: each within the frame bound, and the
    /// set within count and byte budgets the keys message and memory hold. The successor is
    /// expected to hold the rest from the archive; a transfer past these bounds is refused before
    /// the stop, with a message that says to sync the archive first.
    /// Error cases:
    /// - the manifest or the log directory cannot be read
    pub(super) fn transfer_bounds(&self, tip: Option<u64>) -> anyhow::Result<()> {
        const MAX_TAILS: usize = 1024;
        const MAX_TRANSFER_BYTES: u64 = 8 << 30;
        let manifest = Manifest::load(&self.log_dir)?;
        let mut count = 0_usize;
        let mut bytes = 0_u64;
        if let Some(first) = manifest.segments.iter().find(|listed| tip.is_none_or(|tip| listed.last_seq > tip)) {
            ensure!(
                tip.is_none_or(|tip| first.first_seq == tip + 1),
                "the successor's log tip {:?} is not a segment boundary: its copy must be re-synced from the archive",
                tip
            );
        }
        for listed in manifest.segments.iter().filter(|listed| tip.is_none_or(|tip| listed.last_seq > tip)) {
            ensure!(
                listed.bytes <= crate::handover::MAX_SEGMENT_BYTES as u64,
                "the log segment {} is over the handover's 64 MiB frame bound: the successor cannot take it",
                listed.file
            );
            count += 1;
            bytes += listed.bytes;
        }
        ensure!(
            count <= MAX_TAILS && bytes <= MAX_TRANSFER_BYTES,
            "the log past seq {tip:?} is {count} segments, {bytes} bytes: beyond the handover's bounds, so sync              the successor's archive copy first"
        );
        let entries = match std::fs::read_dir(&self.log_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.log_dir.display())),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if crate::log::segment_seq(&name).is_none() || manifest.segments.iter().any(|listed| listed.file == name) {
                continue;
            }
            ensure!(
                entry.metadata()?.len() <= crate::handover::MAX_SEGMENT_BYTES as u64,
                "the open log segment {name} is over the handover's 64 MiB frame bound: rotate it before the handover"
            );
        }
        Ok(())
    }

    /// Refuses `turn` unless this release's log accepted its assignment: the engine refuses a turn
    /// whose key no set gave seven days' notice, and a successor whose key the log never applies must
    /// not take the keys, since this release cannot sequence again once it stops.
    /// Error cases:
    /// - the listener reads no log, the node is stopping, or the log has not accepted the turn
    pub(super) async fn ensure_logged(&self, turn: &ScheduledTurn) -> anyhow::Result<()> {
        let engine = self.engine.as_ref().context("this listener reads no log to check the turn against")?;
        let (number, key) = (turn.turn, turn.admin_key);
        let (reply, answered) = oneshot::channel();
        let query = Box::new(move |engine: &engine_core::Engine| {
            let turns = engine.daily_turns();
            let assigned =
                turns.assigned.get(&number).or(turns.in_effect.as_ref().filter(|in_effect| in_effect.turn == number));
            let _ = reply.send(assigned.is_some_and(|assigned| assigned.admin_key == key));
        });
        engine.send(Request::Query(query)).await.map_err(|_| anyhow!("the node is stopping"))?;
        let accepted = answered.await.map_err(|_| anyhow!("the node is stopping"))?;
        ensure!(
            accepted,
            "this release's log has not accepted turn {number} for key {key}: the engine refused its assignment \
             or has yet to mirror it, so the keys stay here"
        );
        Ok(())
    }
}
