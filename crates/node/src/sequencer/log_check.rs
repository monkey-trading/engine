//! The sequencer's side of the log check (`log_check`): the verified head, the log's `TurnTaken`
//! records that attribute each range, the check due while this node holds a turn, at its boundary
//! when the verified head does not reach it and then at each checkpoint that covers an approval
//! waiting past the verified head, and the gate `release_payouts` applies.

use std::sync::Arc;

use anyhow::Context;
use engine_core::Engine;
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::types::{AccountId, Millis};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::log_check::{Due, Holder, Takings, VerifiedHead, applies, own_candidates, seal_verified};
use crate::sequencer::{CheckWait, Replayed, Sequencer};
use crate::tee::sealing::KeyStore;

/// How often, in sequenced time, the approvals waiting for the installation are counted.
const COUNT_EVERY: Millis = 60_000;

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
    /// The releases the sealed keys name as having handed them on past the verified head, which no
    /// check due may come from.
    holders: Vec<Holder>,
    /// The target of a check that comes due: the latest checkpoint written once one reaches the seq the
    /// next check must (`reach`), fixed once chosen so a check asked for it is still the one due when it
    /// arrives; a restart takes the last checkpoint the log holds.
    target: Option<VerifiedHead>,
    /// The latest checkpoint written, which may be at the `TurnTaken` itself when the schedule wrote
    /// one there before the `TurnTaken` was recorded.
    latest: Option<VerifiedHead>,
    /// The turn `log_check_unattributable` was raised for, until it resolves.
    unattributable: Option<u64>,
    /// Whether `log_check_unavailable` is raised.
    unavailable: bool,
    /// The last drawn turn `log_check_awaiting_installation` was raised for, until it resolves.
    awaiting_installation: Option<u64>,
    /// The last drawn turn whose unpaid approvals were last counted, and the sequenced time they were.
    installation_counted: Option<(u64, Millis)>,
    /// Whether the sealed keys hold a verified head, restored or not.
    sealed: bool,
    /// The seq of the earliest approval due for a signature past the verified head, as the gate's
    /// schedule showed it at the last refresh (`Sequencer::refresh_log_check`).
    waiting: Option<u64>,
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
        let keys = store.keys();
        let (sealed, holders) = (keys.log_check, keys.holders);
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
        Self { operator, store, due, verified, takings, holders, target, latest: target, sealed, ..Self::default() }
    }

    /// Notes the checkpoint `head` just written (see `retarget`).
    fn checkpointed(&mut self, head: VerifiedHead) {
        self.latest = Some(head);
        self.retarget();
    }

    /// Notes the command at `seq`, stamped `ts`, with the `events` it gave (see `command`).
    pub(super) fn applied(&mut self, seq: u64, ts: engine_core::types::Millis, events: &[Event]) {
        self.takings.stamped(seq, ts);
        self.command(seq, events);
    }

    /// Notes the command at `seq` with the `events` it gave (see `Takings::command` and `retarget`).
    pub(super) fn command(&mut self, seq: u64, events: &[Event]) {
        self.takings.command(seq, events);
        self.retarget();
    }

    /// Takes the latest checkpoint as the target while the target does not reach the seq the next check
    /// must (`reach`).
    fn retarget(&mut self) {
        let reach = self.reach(self.takings.last().map_or(0, |(seq, _)| seq));
        if self.target.is_none_or(|target| target.seq < reach) {
            self.target = self.latest;
        }
    }

    /// The seq the next check must verify the log through, after the latest `TurnTaken` at `taken_seq`
    /// (`needed`), or, with none needed, that `TurnTaken`.
    fn reach(&self, taken_seq: u64) -> u64 {
        self.needed(taken_seq).unwrap_or(taken_seq)
    }

    /// The seq a check is needed through, after the latest `TurnTaken` at `taken_seq`: that `TurnTaken`
    /// while the verified head does not reach the command before it, the boundary's check; else the
    /// earliest approval waiting for a signature past the verified head (`waiting`); else none.
    fn needed(&self, taken_seq: u64) -> Option<u64> {
        let verified = self.verified.map(|verified| verified.seq);
        if verified.is_none_or(|verified| verified.saturating_add(1) < taken_seq) {
            return Some(taken_seq);
        }
        self.waiting.filter(|waiting| verified.is_some_and(|verified| *waiting > verified))
    }

    /// The check this node waits for: with the gate applying, while this release holds the turn in effect
    /// and needs a check (`outstanding`), the target checkpoint once it reaches the seq the check needs,
    /// with who sequenced the range since the verified head, the keys' holders included; `None` while no
    /// such checkpoint is written, or while the records cannot attribute the range (`unattributable`).
    fn due(&self, engine: &Engine) -> Option<Due> {
        let (head, in_effect) = self.due_head(engine)?;
        let after = self.verified.map(|verified| verified.seq);
        let first = self.takings.first(after, head.seq);
        let excluded = self.takings.sequenced(after, head.seq)?.with_holders(&self.holders, first);
        let (turn, position, set, candidate) = (in_effect.turn, in_effect.position, in_effect.set, in_effect.candidate);
        Some(Due { turn, position, set, candidate, head, from: first, excluded })
    }

    /// The turn whose check is outstanding, its checkpoint written, while the log's records cannot say
    /// who sequenced its range, as after a start from a snapshot past the verified head: no member can
    /// be asked.
    fn unattributable(&self, engine: &Engine) -> Option<u64> {
        let (head, in_effect) = self.due_head(engine)?;
        let sequenced = self.takings.sequenced(self.verified.map(|verified| verified.seq), head.seq);
        sequenced.is_none().then_some(in_effect.turn)
    }

    /// The target checkpoint of the check outstanding, once it reaches the seq the check needs, and the
    /// turn.
    fn due_head(&self, engine: &Engine) -> Option<(VerifiedHead, engine_core::turns::TurnAssignment)> {
        let (reach, in_effect) = self.outstanding(engine)?;
        Some((self.target.filter(|head| head.seq >= reach)?, in_effect))
    }

    /// The seq a check is needed through (`needed`), and the turn, while this node holds the turn in
    /// effect (`held`).
    fn outstanding(&self, engine: &Engine) -> Option<(u64, engine_core::turns::TurnAssignment)> {
        let (taken_seq, in_effect) = self.held(engine)?;
        Some((self.needed(taken_seq)?, in_effect))
    }

    /// The seq of the `TurnTaken` of the turn in effect, and the turn, while the gate applies and the turn
    /// is this node's operator key's and taken, under this release or an earlier one of the same member
    /// before an upgrade: this node asks for the turn's checks.
    fn held(&self, engine: &Engine) -> Option<(u64, engine_core::turns::TurnAssignment)> {
        let operator = self.operator?;
        let turns = engine.daily_turns();
        let (in_effect, taken) = (turns.in_effect?, turns.taken?);
        let (taken_seq, _) = self.takings.last()?;
        let ours =
            (taken.turn, taken.position) == (in_effect.turn, in_effect.position) && in_effect.admin_key == operator;
        (applies(engine) && ours).then_some((taken_seq, in_effect))
    }

    /// Whether an approval at `approved_seq` may be signed: always while the gate does not apply,
    /// otherwise once the verified head reaches it.
    pub(super) fn covers(&self, engine: &Engine, approved_seq: u64) -> bool {
        !applies(engine) || self.verified.is_some_and(|verified| approved_seq <= verified.seq)
    }

    /// The last drawn turn, the one in effect, while the gate applies, rotation is off and no later turn
    /// is assigned: no boundary follows it, and only the installation that ends the turns lifts the gate.
    fn last_drawn(&self, engine: &Engine) -> Option<u64> {
        let turns = engine.daily_turns();
        let in_effect = turns.in_effect?;
        let later = turns.assigned.range(in_effect.turn.saturating_add(1)..).next().is_some();
        (applies(engine) && !engine.operator_election().rotation_enabled && !later).then_some(in_effect.turn)
    }

    /// What will cover an approval at `approved_seq`, while it waits for a check: the turn in effect's
    /// boundary when nobody has taken it yet, so its member's handover or `TurnTaken` will check this log;
    /// a check this node asks for while it holds the turn, naming the checkpoint of the check due when that
    /// checkpoint covers the approval; else the next turn's boundary, or, after the last drawn turn, the
    /// installation.
    fn wait(&self, engine: &Engine, approved_seq: u64) -> Option<CheckWait> {
        if self.covers(engine, approved_seq) {
            return None;
        }
        let turns = engine.daily_turns();
        let in_effect = turns.in_effect?;
        let untaken =
            turns.taken.is_none_or(|taken| (taken.turn, taken.position) != (in_effect.turn, in_effect.position));
        if untaken {
            let turn = in_effect.turn;
            return Some(CheckWait::Boundary { turn, boundary_ms: engine_core::turns::boundary(turn)? });
        }
        if self.held(engine).is_some() {
            let due = self.due_head(engine).map(|(head, _)| head.seq);
            return Some(CheckWait::Check { checkpoint_seq: due.filter(|seq| *seq >= approved_seq) });
        }
        if let Some(turn) = self.last_drawn(engine) {
            return Some(CheckWait::Installation { turn });
        }
        let turn = in_effect.turn.saturating_add(1);
        Some(CheckWait::Boundary { turn, boundary_ms: engine_core::turns::boundary(turn)? })
    }

    /// Whether the sealed keys may carry the gate's records: a verified head or holders.
    fn carried(&self) -> bool {
        self.sealed || self.verified.is_some() || !self.holders.is_empty()
    }

    /// Drops the verified head, the holders and their seal once the gate no longer applies, so keys
    /// handed over once the turns have ended carry none and a release built before the log check can take
    /// them.
    /// Error cases:
    /// - the keys cannot be sealed again, which leaves the records to drop at the next refresh
    fn forget(&mut self) -> anyhow::Result<()> {
        let Some(store) = &self.store else { return Ok(()) };
        let dropped = |keys: &mut crate::tee::keys::KeySet| {
            keys.log_check = None;
            keys.holders.clear();
        };
        store.update(dropped).context("dropping the gate's records from the sealed keys")?;
        let (seq, holders) = (self.verified.map(|head| head.seq), self.holders.len());
        info!(seq, holders, "the gate no longer applies: dropped its records from the sealed keys");
        (self.verified, self.sealed) = (None, false);
        self.holders.clear();
        Ok(())
    }

    /// Takes `head` as the verified head and seals it with the keys; a failed seal leaves the next
    /// start to ask again.
    fn verify(&mut self, head: VerifiedHead) {
        self.verified = Some(head);
        self.takings.prune(head.seq);
        self.holders.retain(|holder| holder.through > head.seq);
        if let Some(store) = &self.store {
            match store.update(|keys| seal_verified(keys, Some(head))) {
                Ok(()) => self.sealed = true,
                Err(e) => {
                    warn!(seq = head.seq, "sealing the verified head failed: a restart asks for the check again: {e:#}")
                }
            }
        }
    }
}

impl Sequencer {
    /// Publishes the check due, when it changed, from the earliest approval waiting for a signature past
    /// the verified head, and raises `log_check_unattributable` while one is outstanding that no member
    /// can be asked for (`LogCheck::unattributable`), and `log_check_awaiting_installation` while
    /// approvals wait for the installation that ends the turns.
    pub(super) fn refresh_log_check(&mut self) {
        if self.log_check.carried()
            && !applies(&self.engine)
            && let Err(e) = self.log_check.forget()
        {
            warn!("{e:#}: trying again at the next refresh");
        }
        let after = self.log_check.verified.map(|head| head.seq);
        self.log_check.waiting = self.gate.as_ref().and_then(|gate| gate.schedule.first_due_after(after));
        self.log_check.retarget();
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
                    turn, "the log check can be asked for, or is no longer outstanding"
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
        self.refresh_awaiting_installation();
        // Without a setup this node takes no turn and asks for no check, so nothing it holds is signed.
        let unavailable = self.log_check.store.is_none() && applies(&self.engine);
        if std::mem::replace(&mut self.log_check.unavailable, unavailable) != unavailable {
            if unavailable {
                warn!(
                    alert = "log_check_unavailable",
                    "a turn is in effect whose set names two or more members, so withdrawals wait for a log \
                     check, but this node runs without operator.rotation_enabled and sealed keys: it takes no \
                     turn and asks for no check, and signs no withdrawal until it restarts with them"
                );
            } else {
                info!(alert_resolved = "log_check_unavailable", "withdrawals no longer wait for a log check");
            }
        }
    }

    /// Raises `log_check_awaiting_installation`, once for each last drawn turn, while approvals wait for
    /// the installation, and resolves it once none does. The unpaid approvals are counted at most once
    /// a minute of sequenced time, and only after the last drawn turn, so the batches between pay for no
    /// scan.
    fn refresh_awaiting_installation(&mut self) {
        let last = self.log_check.last_drawn(&self.engine);
        if let Some(turn) = last {
            let counted = self.log_check.installation_counted;
            if counted.is_some_and(|(counted, at)| counted == turn && self.last_ts.saturating_sub(at) < COUNT_EVERY) {
                return;
            }
            self.log_check.installation_counted = Some((turn, self.last_ts));
        }
        let held = last.map(|turn| (turn, self.awaiting_installation())).filter(|(_, waiting)| *waiting > 0);
        let raised = std::mem::replace(&mut self.log_check.awaiting_installation, held.map(|(turn, _)| turn));
        if raised == held.map(|(turn, _)| turn) {
            return;
        }
        if let Some(turn) = raised {
            info!(alert_resolved = "log_check_awaiting_installation", turn, "no withdrawal waits for the installation");
        }
        if let Some((turn, waiting)) = held {
            warn!(
                alert = "log_check_awaiting_installation",
                turn,
                "{waiting} approved withdrawals wait for a log check, but rotation is off, no turn after the \
                 last drawn one is assigned, and this node does not hold that turn, so no check it asks for \
                 covers them: they are signed once the timelock's installation of an operator ends the turns"
            );
        }
    }

    /// Notes the checkpoint just written at `seq` with `root`, the latest, which a check that comes due
    /// targets when the target falls short of the seq the check needs: at the boundary, the latest
    /// `TurnTaken`; within the turn, the earliest approval waiting past the verified head.
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

    /// Names this release, with the candidates its operator key sits for in the log's turns, as a holder
    /// of the keys through the last command, a handover's final checkpoint, in the sealed keys the
    /// handover sends, while the gate applies to the log there (`log_check::Holder`): the log names no
    /// member whose skip a reorg removed, and no check counts from a holder for a range reaching its
    /// commands. While the gate does not apply, the keys the handover sends carry none of its records,
    /// so a release built before them takes the keys.
    /// Error cases:
    /// - the keys cannot be sealed again, which refuses the handover
    pub(super) fn log_check_hand_over(&mut self) -> anyhow::Result<()> {
        if !applies(&self.engine) {
            return if self.log_check.carried() { self.log_check.forget() } else { Ok(()) };
        }
        let check = &mut self.log_check;
        let (Some(store), Some(operator), Some(through)) = (&check.store, check.operator, self.engine.last_seq())
        else {
            return Ok(());
        };
        let release = B256::from(store.release().id());
        let holder = Holder { through, release, candidates: own_candidates(&self.engine, operator) };
        if check.holders.contains(&holder) {
            return Ok(());
        }
        store.update(|keys| keys.holders.push(holder.clone())).context("naming this release a holder of the keys")?;
        info!(through, "the keys name this release as a holder through the handover's final checkpoint");
        check.holders.push(holder);
        Ok(())
    }

    /// What an approval at `approved_seq` waits for (see `LogCheck::wait`).
    pub(super) fn log_check_wait(&self, approved_seq: u64) -> Option<CheckWait> {
        self.log_check.wait(&self.engine, approved_seq)
    }
}

#[cfg(test)]
mod tests;
