//! The sequencer's side of the boundary's log check (`log_check`): the verified head, the log's
//! `TurnTaken` records that attribute each range, the check due once this node takes a turn its
//! verified head does not cover, and the gate `release_payouts` applies.

use std::sync::Arc;

use engine_core::Engine;
use engine_core::eip712::B256;
use engine_core::event::Event;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::log_check::{Due, Takings, VerifiedHead, applies};
use crate::sequencer::{CheckWait, Replayed, Sequencer};
use crate::tee::sealing::KeyStore;

/// What a node that takes turns gives its sequencer for the log check.
pub(crate) struct LogCheckSetup {
    /// This enclave's release, which takes the node's turns.
    pub release: B256,
    /// The sealed keys the verified head is kept with.
    pub store: Arc<KeyStore>,
    /// Where the check due is published, for the node's asking (`log_check::ask`).
    pub due: watch::Sender<Option<Due>>,
}

/// The verified head and what decides the next check.
#[derive(Default)]
pub(super) struct LogCheck {
    release: Option<B256>,
    store: Option<Arc<KeyStore>>,
    due: Option<watch::Sender<Option<Due>>>,
    verified: Option<VerifiedHead>,
    takings: Takings,
    /// The first checkpoint written at or after the latest `TurnTaken`, the target of a check that
    /// comes due, fixed once chosen so a check asked for it is still the one due when it arrives; a
    /// restart takes the last checkpoint the log holds.
    target: Option<VerifiedHead>,
    /// The latest checkpoint written, which may be at the `TurnTaken` itself when the schedule wrote
    /// one there before the `TurnTaken` was recorded.
    latest: Option<VerifiedHead>,
}

impl LogCheck {
    /// The log check of the log `replayed` recovered, under `setup`: the verified head the sealed keys
    /// hold, kept only when the log holds that checkpoint, since a host can hand a start another log.
    pub(super) fn recovered(setup: Option<LogCheckSetup>, replayed: &Replayed) -> Self {
        let next = replayed.engine.last_seq().map_or(0, |seq| seq.saturating_add(1));
        let taken = replayed.engine.daily_turns().taken.map(|taken| taken.release);
        let takings = replayed.takings.clone().complete_from(next, taken);
        let target = replayed.last_signed_checkpoint.as_ref().and_then(|checkpoint| {
            let link = replayed.prefix_ring.prefix_at(checkpoint.seq)?;
            Some(VerifiedHead { seq: checkpoint.seq, link, root: checkpoint.root.parse().ok()? })
        });
        let Some(LogCheckSetup { release, store, due }) = setup else {
            return Self { takings, target, latest: target, ..Self::default() };
        };
        let sealed = store.keys().log_check;
        let verified = sealed.filter(|head| {
            replayed.checkpoint_roots.get(&head.seq) == Some(&head.root)
                && replayed.prefix_ring.prefix_at(head.seq).is_none_or(|link| link == head.link)
        });
        match (sealed, verified) {
            (Some(sealed), None) => warn!(
                seq = sealed.seq,
                "the sealed verified head is not a checkpoint of this log: payouts wait for the next log check"
            ),
            (_, Some(verified)) => info!(seq = verified.seq, "restored the head the last log check verified"),
            (None, None) => {}
        }
        let (store, due) = (Some(store), Some(due));
        Self { release: Some(release), store, due, verified, takings, target, latest: target }
    }

    /// Notes the checkpoint `head` just written (see `retarget`).
    fn checkpointed(&mut self, head: VerifiedHead) {
        self.latest = Some(head);
        self.retarget();
    }

    /// Notes the command at `seq` with the `events` it gave, after which the engine records `taken` as
    /// the last release to take a turn (see `Takings::command` and `retarget`).
    fn command(&mut self, seq: u64, events: &[Event], taken: Option<B256>) {
        self.takings.command(seq, events, taken);
        self.retarget();
    }

    /// Takes the latest checkpoint as the target while none at or after the latest `TurnTaken` is.
    fn retarget(&mut self) {
        let taken = self.takings.last().map_or(0, |(seq, _)| seq);
        if self.target.is_none_or(|target| target.seq < taken) {
            self.target = self.latest;
        }
    }

    /// The check this node waits for: with the gate applying, once this release has taken the turn in
    /// effect at a `TurnTaken` its verified head does not reach, the target checkpoint at or after it,
    /// with the releases that sequenced the range since the verified head; `None` while no checkpoint
    /// has followed, or while the records cannot attribute the range, as after a snapshot start.
    fn due(&self, engine: &Engine) -> Option<Due> {
        let (taken_seq, in_effect) = self.outstanding(engine)?;
        let head = self.target.filter(|head| head.seq >= taken_seq)?;
        let Some(excluded) = self.takings.sequenced(self.verified.map(|verified| verified.seq), head.seq) else {
            debug!(seq = head.seq, "the log's records do not attribute the range a check would cover");
            return None;
        };
        let (turn, position, set, candidate) = (in_effect.turn, in_effect.position, in_effect.set, in_effect.candidate);
        Some(Due { turn, position, set, candidate, head, excluded })
    }

    /// The seq of this release's `TurnTaken` of the turn in effect, and the turn, while the gate
    /// applies and the verified head does not reach the command before it.
    fn outstanding(&self, engine: &Engine) -> Option<(u64, engine_core::turns::TurnAssignment)> {
        let release = self.release?;
        let turns = engine.daily_turns();
        let (in_effect, taken) = (turns.in_effect?, turns.taken?);
        let (taken_seq, taken_release) = self.takings.last()?;
        let ours = (taken.turn, taken.position) == (in_effect.turn, in_effect.position)
            && taken.release == release
            && taken_release == release;
        let covered = self.verified.is_some_and(|verified| verified.seq.saturating_add(1) >= taken_seq);
        (applies(engine) && ours && !covered).then_some((taken_seq, in_effect))
    }

    /// Whether an approval at `approved_seq` may be signed: always while the gate does not apply,
    /// otherwise once the verified head reaches it.
    pub(super) fn covers(&self, engine: &Engine, approved_seq: u64) -> bool {
        !applies(engine) || self.verified.is_some_and(|verified| approved_seq <= verified.seq)
    }

    /// The boundary whose check will cover an approval at `approved_seq`, while it waits for one:
    /// the turn in effect's when its check is outstanding and will reach it, its target not yet
    /// written or at or past the approval, else the next turn's.
    fn wait(&self, engine: &Engine, approved_seq: u64) -> Option<CheckWait> {
        if self.covers(engine, approved_seq) {
            return None;
        }
        let in_effect = engine.daily_turns().in_effect?;
        let this_turn = self.outstanding(engine).is_some_and(|(taken_seq, _)| {
            let target = self.target.filter(|target| target.seq >= taken_seq);
            approved_seq < taken_seq || target.is_none_or(|target| target.seq >= approved_seq)
        });
        let turn = if this_turn { in_effect.turn } else { in_effect.turn.saturating_add(1) };
        Some(CheckWait { turn, boundary_ms: engine_core::turns::boundary(turn)? })
    }

    /// Takes `head` as the verified head and seals it with the keys; a failed seal leaves the next
    /// start to ask again.
    fn verify(&mut self, head: VerifiedHead) {
        self.verified = Some(head);
        self.takings.prune(head.seq);
        if let Some(store) = &self.store
            && let Err(e) = store.update(|keys| keys.log_check = Some(head))
        {
            warn!(seq = head.seq, "sealing the verified head failed: a restart asks for the check again: {e:#}");
        }
    }
}

impl Sequencer {
    /// Publishes the check due, when it changed.
    pub(super) fn refresh_log_check(&mut self) {
        let due = self.log_check.due(&self.engine);
        if let Some(sender) = &self.log_check.due {
            sender.send_if_modified(|current| {
                let changed = *current != due;
                *current = due;
                changed
            });
        }
    }

    /// Notes the checkpoint just written at `seq` with `root`, which a check that comes due targets
    /// when it is the first at or after the latest `TurnTaken`.
    pub(super) fn log_check_checkpointed(&mut self, seq: u64, root: [u8; 32]) {
        let link = B256::from(self.command_prefix);
        self.log_check.checkpointed(VerifiedHead { seq, link, root: B256::from(root) });
        self.refresh_log_check();
    }

    /// Records this node's `TurnTaken` at `seq`, and, when its verified head does not reach the
    /// boundary, writes the checkpoint the check verifies, held back while the last is not final, unless
    /// the schedule already wrote one at `seq`.
    /// Error cases:
    /// - the checkpoint cannot be written
    pub(super) fn log_check_taken(&mut self, seq: u64, events: &[Event]) -> anyhow::Result<()> {
        let taken = self.engine.daily_turns().taken.map(|taken| taken.release);
        self.log_check.command(seq, events, taken);
        if self.log_check.outstanding(&self.engine).is_some() {
            info!(seq, "the log check at this boundary is outstanding: checkpointing for a member to verify");
            self.paced_checkpoint()?;
        }
        self.refresh_log_check();
        Ok(())
    }

    /// Takes a member's verification of `head` from `release`, which this node admitted: only while it
    /// is the check due and `release` sequenced none of its range. Whether it was taken.
    pub(super) fn log_checked(&mut self, head: VerifiedHead, release: B256) -> bool {
        let due = self.log_check.due(&self.engine);
        let Some(due) = due.filter(|due| due.head == head && !due.excluded.contains(&release)) else {
            warn!(seq = head.seq, %release, "a verification that is not the check due, or from a release in its range");
            return false;
        };
        self.log_check.verify(head);
        info!(seq = head.seq, turn = due.turn, %release, "a member's check verified the log through seq");
        self.refresh_log_check();
        true
    }

    /// The boundary whose check an approval at `approved_seq` waits for (see `LogCheck::wait`).
    pub(super) fn log_check_wait(&self, approved_seq: u64) -> Option<CheckWait> {
        self.log_check.wait(&self.engine, approved_seq)
    }
}

#[cfg(test)]
mod tests;
