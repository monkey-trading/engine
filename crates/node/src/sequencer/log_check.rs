//! The sequencer's side of the boundary's log check (`log_check`): the verified head, the log's
//! `TurnTaken` records that attribute each range, the check due once this node takes a turn its
//! verified head does not cover, and the gate `release_payouts` applies.

use std::sync::Arc;

use engine_core::Engine;
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::types::AccountId;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::log_check::{Due, Takings, VerifiedHead, applies};
use crate::sequencer::{CheckWait, Replayed, Sequencer};
use crate::tee::sealing::KeyStore;

/// What a node that takes turns gives its sequencer for the log check.
pub(crate) struct LogCheckSetup {
    /// The sealed keys the verified head is kept with.
    pub store: Arc<KeyStore>,
    /// Where the check due is published, for the node's asking (`log_check::ask`).
    pub due: watch::Sender<Option<Due>>,
}

/// The verified head and what decides the next check.
#[derive(Default)]
pub(super) struct LogCheck {
    /// With a setup, this node's operator key, whose turns it takes under any release.
    operator: Option<AccountId>,
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
    /// The turn `log_check_unattributable` was raised for, until it resolves.
    unattributable: Option<u64>,
    /// Whether `log_check_unavailable` is raised.
    unavailable: bool,
    /// Whether the sealed keys hold a verified head, restored or not.
    sealed: bool,
}

impl LogCheck {
    /// The log check of the log `replayed` recovered, under `setup` for the turns of the `operator`
    /// key: the verified head the sealed keys hold, kept only when the log holds that checkpoint, since
    /// a host can hand a start another log.
    pub(super) fn recovered(setup: Option<LogCheckSetup>, operator: AccountId, replayed: &Replayed) -> Self {
        let next = replayed.engine.last_seq().map_or(0, |seq| seq.saturating_add(1));
        let takings = replayed.takings.clone().complete_from(next);
        let target = replayed.last_signed_checkpoint.as_ref().and_then(|checkpoint| {
            let link = replayed.prefix_ring.prefix_at(checkpoint.seq)?;
            Some(VerifiedHead { seq: checkpoint.seq, link, root: checkpoint.root.parse().ok()? })
        });
        let Some(LogCheckSetup { store, due }) = setup else {
            return Self { takings, target, latest: target, ..Self::default() };
        };
        let sealed = store.keys().log_check;
        let verified = sealed.filter(|head| {
            replayed.checkpoint_root(head.seq) == Some(head.root)
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
        let (operator, store, due) = (Some(operator), Some(store), Some(due));
        let sealed = sealed.is_some();
        Self { operator, store, due, verified, takings, target, latest: target, sealed, ..Self::default() }
    }

    /// Notes the checkpoint `head` just written (see `retarget`).
    fn checkpointed(&mut self, head: VerifiedHead) {
        self.latest = Some(head);
        self.retarget();
    }

    /// Notes the command at `seq` with the `events` it gave (see `Takings::command` and `retarget`).
    pub(super) fn command(&mut self, seq: u64, events: &[Event]) {
        self.takings.command(seq, events);
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
    /// with who sequenced the range since the verified head; `None` while no checkpoint has followed,
    /// or while the records cannot attribute the range (`unattributable`).
    fn due(&self, engine: &Engine) -> Option<Due> {
        let (taken_seq, in_effect) = self.outstanding(engine)?;
        let head = self.target.filter(|head| head.seq >= taken_seq)?;
        let excluded = self.takings.sequenced(self.verified.map(|verified| verified.seq), head.seq)?;
        let (turn, position, set, candidate) = (in_effect.turn, in_effect.position, in_effect.set, in_effect.candidate);
        Some(Due { turn, position, set, candidate, head, excluded })
    }

    /// The turn whose check is outstanding, its checkpoint written, while the log's records cannot say
    /// who sequenced its range, as after a start from a snapshot past the verified head: no member can
    /// be asked.
    fn unattributable(&self, engine: &Engine) -> Option<u64> {
        let (taken_seq, in_effect) = self.outstanding(engine)?;
        let head = self.target.filter(|head| head.seq >= taken_seq)?;
        let sequenced = self.takings.sequenced(self.verified.map(|verified| verified.seq), head.seq);
        sequenced.is_none().then_some(in_effect.turn)
    }

    /// The seq of the `TurnTaken` of the turn in effect, and the turn, while the turn is this node's
    /// operator key's and taken, under this release or an earlier one of the same member before an
    /// upgrade, the gate applies, and the verified head does not reach the command before it.
    fn outstanding(&self, engine: &Engine) -> Option<(u64, engine_core::turns::TurnAssignment)> {
        let operator = self.operator?;
        let turns = engine.daily_turns();
        let (in_effect, taken) = (turns.in_effect?, turns.taken?);
        let (taken_seq, _) = self.takings.last()?;
        let ours =
            (taken.turn, taken.position) == (in_effect.turn, in_effect.position) && in_effect.admin_key == operator;
        let covered = self.verified.is_some_and(|verified| verified.seq.saturating_add(1) >= taken_seq);
        (applies(engine) && ours && !covered).then_some((taken_seq, in_effect))
    }

    /// Whether an approval at `approved_seq` may be signed: always while the gate does not apply,
    /// otherwise once the verified head reaches it.
    pub(super) fn covers(&self, engine: &Engine, approved_seq: u64) -> bool {
        !applies(engine) || self.verified.is_some_and(|verified| approved_seq <= verified.seq)
    }

    /// The boundary whose check will cover an approval at `approved_seq`, while it waits for one:
    /// the turn in effect's when nobody has taken it yet, so its member's handover will check this
    /// log, or when its check is outstanding and will reach the approval, its target not yet written
    /// or at or past it; else the next turn's.
    fn wait(&self, engine: &Engine, approved_seq: u64) -> Option<CheckWait> {
        if self.covers(engine, approved_seq) {
            return None;
        }
        let turns = engine.daily_turns();
        let in_effect = turns.in_effect?;
        let untaken =
            turns.taken.is_none_or(|taken| (taken.turn, taken.position) != (in_effect.turn, in_effect.position));
        let this_turn = untaken
            || self.outstanding(engine).is_some_and(|(taken_seq, _)| {
                let target = self.target.filter(|target| target.seq >= taken_seq);
                approved_seq < taken_seq || target.is_none_or(|target| target.seq >= approved_seq)
            });
        let turn = if this_turn { in_effect.turn } else { in_effect.turn.saturating_add(1) };
        Some(CheckWait { turn, boundary_ms: engine_core::turns::boundary(turn)? })
    }

    /// Drops the verified head and its seal once the gate no longer applies, so keys handed over with
    /// rotation off carry none and a release built before the log check can take them; a failed seal
    /// is tried again at the next refresh.
    fn forget(&mut self) {
        let Some(store) = &self.store else { return };
        match store.update(|keys| keys.log_check = None) {
            Ok(()) => {
                info!(
                    seq = self.verified.map(|head| head.seq),
                    "the gate no longer applies: dropped the verified head"
                );
                (self.verified, self.sealed) = (None, false);
            }
            Err(e) => warn!("dropping the verified head from the sealed keys failed: {e:#}"),
        }
    }

    /// Takes `head` as the verified head and seals it with the keys; a failed seal leaves the next
    /// start to ask again.
    fn verify(&mut self, head: VerifiedHead) {
        self.verified = Some(head);
        self.takings.prune(head.seq);
        if let Some(store) = &self.store {
            match store.update(|keys| keys.log_check = Some(head)) {
                Ok(()) => self.sealed = true,
                Err(e) => {
                    warn!(seq = head.seq, "sealing the verified head failed: a restart asks for the check again: {e:#}")
                }
            }
        }
    }
}

impl Sequencer {
    /// Publishes the check due, when it changed, and raises `log_check_unattributable` while one is
    /// outstanding that no member can be asked for (`LogCheck::unattributable`).
    pub(super) fn refresh_log_check(&mut self) {
        if (self.log_check.sealed || self.log_check.verified.is_some()) && !applies(&self.engine) {
            self.log_check.forget();
        }
        let due = self.log_check.due(&self.engine);
        if let Some(sender) = &self.log_check.due {
            sender.send_if_modified(|current| {
                let changed = *current != due;
                *current = due;
                changed
            });
        }
        let unattributable = self.log_check.unattributable(&self.engine);
        let raised = std::mem::replace(&mut self.log_check.unattributable, unattributable);
        if raised != unattributable {
            if let Some(turn) = raised {
                info!(
                    alert_resolved = "log_check_unattributable",
                    turn, "the boundary's log check can be asked for, or is no longer outstanding"
                );
            }
            if let Some(turn) = unattributable {
                warn!(
                    alert = "log_check_unattributable",
                    turn,
                    "the log's records do not reach back to the last verified head, as after a start from a snapshot: \
                     no member can be asked to check the log, and withdrawals approved since that head wait for a \
                     handover to another member; restart without [snapshots] to replay the whole log"
                );
            }
        }
        // Without a setup this node takes no turn and asks for no check, so nothing it holds is signed.
        let unavailable = self.log_check.store.is_none() && applies(&self.engine);
        if std::mem::replace(&mut self.log_check.unavailable, unavailable) != unavailable {
            if unavailable {
                warn!(
                    alert = "log_check_unavailable",
                    "rotation is on and the turn's set names two or more members, so withdrawals wait for the \
                     boundary's log check, but this node runs without operator.rotation_enabled and sealed keys: it \
                     takes no turn and asks for no check, and signs no withdrawal until it restarts with them"
                );
            } else {
                info!(
                    alert_resolved = "log_check_unavailable",
                    "withdrawals no longer wait for the boundary's log check"
                );
            }
        }
    }

    /// Notes the checkpoint just written at `seq` with `root`, which a check that comes due targets
    /// when it is the first at or after the latest `TurnTaken`.
    pub(super) fn log_check_checkpointed(&mut self, seq: u64, root: [u8; 32]) {
        let link = B256::from(self.command_prefix);
        self.log_check.checkpointed(VerifiedHead { seq, link, root: B256::from(root) });
        self.refresh_log_check();
    }

    /// After this node's `TurnTaken` at `seq`, which its stamp recorded, writes the checkpoint the
    /// check verifies when its verified head does not reach the boundary, held back while the last is
    /// not final, unless the schedule already wrote one at `seq`.
    /// Error cases:
    /// - the checkpoint cannot be written
    pub(super) fn log_check_taken(&mut self, seq: u64) -> anyhow::Result<()> {
        if self.log_check.outstanding(&self.engine).is_some() {
            info!(seq, "the log check at this boundary is outstanding: checkpointing for a member to verify");
            self.paced_checkpoint()?;
        }
        self.refresh_log_check();
        Ok(())
    }

    /// Takes a verification of `head` by `candidate`'s member on `release`, which this node admitted:
    /// only while it is the check due and neither sequenced any of its range. Whether it was taken.
    pub(super) fn log_checked(&mut self, head: VerifiedHead, (candidate, release): (AccountId, B256)) -> bool {
        let due = self.log_check.due(&self.engine);
        let Some(due) = due.filter(|due| due.head == head && !due.excluded.include(candidate, release)) else {
            warn!(
                seq = head.seq,
                verifier = %candidate,
                %release,
                "a verification that is not the check due, or from a member or release in its range"
            );
            return false;
        };
        self.log_check.verify(head);
        info!(seq = head.seq, turn = due.turn, verifier = %candidate, %release, "a member's check verified the log through seq");
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
