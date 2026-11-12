//! The predecessor's checks before it stops: whether the tail fits the handover, and whether its
//! own log accepted the daily turn a successor takes, or, once it has stopped, will accept it.

use anyhow::{Context, anyhow, ensure};
use engine_core::turns::DailyTurns;
use engine_core::types::AccountId;
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
    /// not take the keys, since this release cannot sequence again once it stops. The answer for the
    /// turn admitted is kept, so a successor retrying after the stop, when the log is final and the
    /// sequencer gone, is admitted under it. Once the sequencer is gone, the turns retained at the stop
    /// answer for any other turn: the log accepted it, or will when the successor continuing it mirrors
    /// the assignment, as the set it was drawn from names its member with seven days' notice by its
    /// boundary.
    /// Error cases:
    /// - the listener reads no log, the node is stopping with no turns retained, or the log has not
    ///   accepted the turn and, once stopped, would not
    pub(super) async fn ensure_logged(&self, turn: &ScheduledTurn) -> anyhow::Result<()> {
        let (number, key) = (turn.turn, turn.admin_key);
        if *self.logged.lock().expect("the logged turn lock") == Some((number, key)) {
            return Ok(());
        }
        let accepted = match self.query(move |engine| logged(engine.daily_turns(), number, key)).await {
            Ok(accepted) => accepted,
            Err(e) => match self.retained.lock().expect("the retained turns lock").as_ref() {
                Some(retained) => {
                    logged(retained, number, key)
                        || retained.set_refusal(turn.set, turn.candidate, key, turn.boundary).is_none()
                }
                None => return Err(e),
            },
        };
        ensure!(
            accepted,
            "this release's log has not accepted turn {number} for key {key}: the engine refused its assignment \
             or has yet to mirror it, so the keys stay here"
        );
        *self.logged.lock().expect("the logged turn lock") = Some((number, key));
        Ok(())
    }

    /// Retains the log's daily turns as sequencing stops, which answer for a later turn's member once
    /// the sequencer is gone (`ensure_logged`). Read just before the stop: a set mirrored in between is
    /// missing, which only refuses, and a read that fails retains none, which refuses such a member.
    pub(super) async fn retain_turns(&self) {
        let retained = self.query(|engine| engine.daily_turns().clone()).await;
        *self.retained.lock().expect("the retained turns lock") = retained.ok();
    }

    /// Runs `read` on the sequencer's engine.
    /// Error cases:
    /// - the listener reads no log, or the node is stopping
    async fn query<T: Send + 'static>(
        &self,
        read: impl FnOnce(&engine_core::Engine) -> T + Send + 'static,
    ) -> anyhow::Result<T> {
        let engine = self.engine.as_ref().context("this listener reads no log to check the turn against")?;
        let engine = engine.upgrade().ok_or_else(|| anyhow!("the node is stopping"))?;
        let (reply, answered) = oneshot::channel();
        let query = Box::new(move |engine: &engine_core::Engine| {
            let _ = reply.send(read(engine));
        });
        engine.send(Request::Query(query)).await.map_err(|_| anyhow!("the node is stopping"))?;
        answered.await.map_err(|_| anyhow!("the node is stopping"))
    }
}

/// Whether `turns` hold turn `number`'s accepted assignment for `key`, waiting or in effect.
fn logged(turns: &DailyTurns, number: u64, key: AccountId) -> bool {
    let in_effect = turns.in_effect.as_ref().filter(|in_effect| in_effect.turn == number);
    turns.assigned.get(&number).or(in_effect).is_some_and(|assigned| assigned.admin_key == key)
}
