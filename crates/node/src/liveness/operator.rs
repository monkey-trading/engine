//! The operator's side of the liveness rule. It reads the chain at the latest agreed block four times a
//! heartbeat interval, or every `[liveness] read_interval` while it knows no interval, and once trusted
//! time reaches the turn boundary after the last read's block; every `POLL` it asks the members under the last read
//! and, while a turn names the operator and its set has another member:
//! - asks each other member for its status, over a fresh nonce, and takes a status that recovers to
//!   the member's promotable standby key, or, while it has none, to the key whose attestation this
//!   enclave verified (`attested`), names this signer epoch, and holds a link this log holds at that
//!   seq, as that member's confirmation;
//! - lets the poster sign at the newest confirmed seq only (`Signing::Confirmed`);
//! - holds sequencing until a start check passes: a member whose copy holds a command answered, or,
//!   with this log empty at genesis, any member did, an empty copy's included, and none holds a seq
//!   past this log or a link that conflicts with it, which raises Critical `liveness_start_refused`
//!   naming the range;
//! - stops taking commands once the log has run more than one heartbeat interval of sequenced time from
//!   the first command past the newest confirmation (Critical `liveness_unconfirmed`);
//! - feeds the fence the read's `promotionDueAt`, which a separate tick checks against trusted time.
//!
//! A read that fails, or comes from a block older than a promotion's, still asks the members of the
//! last read the rule applied under: only the fence stops the log for want of reads. A confirmation
//! counts only under the signer epoch, schedule and set it was given in. Before any exemption, a vault
//! naming another key than this enclave's ends the run.
//!
//! While no turn names the operator, or its set has no other member, the gate is open and the poster
//! signs at the log's tip, until a turn could let a promotion pass, unless a later read moves it on. A missing
//! confirmation is never a hold: the heartbeat simply stops.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256};
use engine_core::types::AccountId;
use tokio::sync::{mpsc, watch};
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, error, info, warn};

use crate::config::TeeProvider;
use crate::liveness::chain::{Member, View, first_turnable};
use crate::liveness::fence::Fence;
use crate::liveness::link::Asking;
use crate::liveness::operator::fencing::{AbortOnDrop, FenceState, fence_tick, lock};
use crate::liveness::{Gate, Released, Signable, VaultSigner};
use crate::seed_relay::schedule::next_boundary;
use crate::sequencer::Request as SequencerRequest;
use crate::standby_key::chain::StandbyChain;
use crate::tee::sealing::Platform;
use crate::trusted_time::TrustedClock;

/// How often the operator asks the members.
pub(crate) const POLL: Duration = Duration::from_secs(1);
/// How often the fence is checked against trusted time.
pub(super) const FENCE_TICK: Duration = if cfg!(test) { Duration::from_millis(10) } else { Duration::from_millis(250) };
/// How long a member gets to answer a status question.
const STATUS_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a set's members are read again.
const MEMBERS_EVERY: Duration = Duration::from_secs(60);
/// How soon the operator asks again while a checkpoint waits for a confirmation.
const HURRY: Duration = Duration::from_millis(25);

/// Where the poster may sign.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Signing {
    /// Nothing yet: no read has said whether the rule applies, or no member confirmed a head.
    #[default]
    Nothing,
    /// The log's tip: the rule does not apply, or the set has no other member, whose signer epoch and
    /// set it names (`None` with no turn naming the operator): a head is signed at the tip only under
    /// that read's turn, so a change of set the rule has not read signs nothing unconfirmed, and only
    /// before the time in milliseconds from which another set may apply (`Signable::until`).
    Tip(Option<(u64, u32)>, u64),
    /// The newest seq a member confirmed.
    Confirmed(Signable),
}

impl Signing {
    /// The time in milliseconds from which nothing is signed at it: its own boundary from which another
    /// set may apply, whatever later read renewed the gate's.
    pub(crate) fn until(&self) -> u64 {
        match self {
            Signing::Nothing => 0,
            Signing::Tip(_, until) => *until,
            Signing::Confirmed(signable) => signable.until,
        }
    }
}

/// What one round of statuses shows against this log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Assessment {
    /// How many members answered with a verified status of a copy holding a command, which this log's
    /// prefix ring checks.
    pub answered: usize,
    /// How many answered with a verified status of a copy holding none.
    pub empty: usize,
    /// The newest seq a member's durable copy holds with this log's link there, and that member.
    pub confirmed: Option<(u64, AccountId)>,
    /// The highest seq a member holds past this log's tip, and that member.
    pub beyond: Option<(u64, AccountId)>,
    /// A seq where a member holds another link than this log, and that member.
    pub conflict: Option<(u64, AccountId)>,
}

/// Which members a read lists: the schedule's, of a set, for a turn.
type SetId = (Address, u32, u64);

/// Whose confirmations count: a signer epoch's, under a schedule, from one of its sets' members.
type Scope = (u64, Address, u32);

/// The last read the rule applied under: its view, whose confirmations count, and the members it asks.
type Applied = (View, Scope, Vec<Member>);

/// The candidate that registered each standby key a member's tail was sealed by, as the schedule
/// reads, `None` for a key none registered (the turn's keys): a key's holder never changes.
pub(super) type Holders = BTreeMap<AccountId, Option<AccountId>>;

/// What the rounds carry from one to the next: the released heads' stamps, the newest confirmation and
/// the standby keys' holders.
type Rounds<'a> = (&'a mut Stamps, &'a mut Option<(Scope, Signable)>, &'a mut Holders);

/// The operator's liveness rule.
pub(crate) struct Operator<C, A> {
    pub chain: C,
    pub asking: A,
    /// The deployment: its chain id and vault, whose registry and schedule each read resolves.
    pub chain_id: u64,
    pub vault: Address,
    /// The LogHeads contract the heads go to.
    pub log_heads: Address,
    /// This enclave's address, which the vault must name.
    pub key: AccountId,
    /// This node's admin key: the member it runs for, which it never asks for a confirmation.
    pub admin: AccountId,
    pub gate: Gate,
    pub trusted: TrustedClock,
    pub released: watch::Receiver<Released>,
    pub signing: watch::Sender<Signing>,
    /// The ticker's interval, which must be at most half the heartbeat interval.
    pub tick: Duration,
    /// The sequencer, whose prefix ring holds this log's cumulative prefix at every recent seq.
    pub sequencer: mpsc::WeakSender<SequencerRequest>,
    /// The vault's signer as each read finds it, which `/v1/operator` names.
    pub vault_signer: watch::Sender<Option<VaultSigner>>,
    /// How often it asks the members: `POLL`.
    pub poll: Duration,
    /// Its read period while it knows no heartbeat interval: `[liveness] read_interval`.
    pub read_every: Duration,
    /// For a promoted operator, the block its start read the promotion at: an older latest block, as a
    /// lagging provider majority serves, still names the outgoing key, so it counts as no read.
    pub floor: u64,
    /// This enclave's attestation provider and sealed platform, which a member's standby key's
    /// attestation is checked against; `None` without sealed keys, when only promotable keys count.
    pub platform: Option<(TeeProvider, Platform)>,
    /// The attested keys it verified, which a failed re-check keeps (`attested`).
    pub kept: attested::Kept,
}

impl<C: StandbyChain, A: Asking> Operator<C, A> {
    /// Runs the rule until the released-head watch closes.
    /// Error cases:
    /// - none: failed reads and silent members are retried
    pub(crate) async fn run(mut self) -> anyhow::Result<()> {
        let state = Arc::new(Mutex::new(FenceState::default()));
        let fencing = tokio::spawn(fence_tick(state.clone(), self.gate.clone(), self.trusted.clone()));
        let _abort = AbortOnDrop(fencing);
        let mut poll = interval(self.poll);
        poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let (mut stamps, sampling) = Stamps::sample(self.released.clone());
        let _sampling = AbortOnDrop(sampling);
        let mut set: Option<(SetId, tokio::time::Instant, Vec<Member>)> = None;
        let mut confirmed: Option<(Scope, Signable)> = None;
        let mut applied: Option<Applied> = None;
        let mut holders = Holders::new();
        let mut episodes = Episodes::default();
        // When the chain was last read, and the last successful read's block time and heartbeat interval.
        let (mut read_at, mut last): (Option<tokio::time::Instant>, Option<(u64, u64)>) = (None, None);
        loop {
            // A checkpoint that waits for a confirmation wakes the rule at once.
            let woken = tokio::select! {
                _ = poll.tick() => false,
                () = self.gate.woken() => true,
            };
            if self.released.has_changed().is_err() {
                return Ok(());
            }
            stamps.with(*self.released.borrow_and_update());
            // It asks the last read's members at once: a chain read first would add to the pause.
            if let Some(current) = applied.as_ref().filter(|_| woken) {
                self.round(current, (&mut stamps, &mut confirmed, &mut holders), &mut episodes).await;
                continue;
            }
            // Between reads the members confirm under the last read the rule applied under.
            let reached = self.trusted.reached_ms().and_then(|now| u64::try_from(now).ok());
            // A read is due on the tick nearest its time, so a read every 15 s lands every 15 s, not 16.
            let since = read_at.map(|at| at.elapsed() + self.poll / 2);
            if !read_due(self.read_every, since, last, reached) {
                self.unread(applied.as_ref(), (&mut stamps, &mut confirmed, &mut holders), &mut episodes).await;
                continue;
            }
            read_at = Some(tokio::time::Instant::now());
            let cached = set
                .as_ref()
                .filter(|(_, at, _)| at.elapsed() < MEMBERS_EVERY)
                .map(|(id, _, listed)| (*id, listed.clone()));
            let cached_set = cached.as_ref().map(|(id, _)| *id);
            let read = self.read(cached);
            let read = match read.await {
                Ok(read) => {
                    resolve(&mut episodes.read_failed, "liveness_read_failed", "reading the chain works again");
                    read
                }
                Err(e) => {
                    if !std::mem::replace(&mut episodes.read_failed, true) {
                        warn!(alert = "liveness_read_failed", "reading the liveness rule failed: {e:#}");
                    }
                    // The members still confirm under the last read the rule applied under: only the
                    // fence, from that read's E, stops the log for want of reads.
                    let rounds = (&mut stamps, &mut confirmed, &mut holders);
                    self.unread(applied.as_ref(), rounds, &mut episodes).await;
                    continue;
                }
            };
            let (schedule, view, listed, turnable) = match read {
                Read::Unscheduled { time, signer } => {
                    last = Some((time, 0));
                    if !std::mem::replace(&mut episodes.unscheduled, true) {
                        info!("the vault's registry names no schedule: the liveness rule does not apply");
                    }
                    self.named(&state, signer);
                    applied = None;
                    let turnable = first_turnable(time, None);
                    self.exempt(&mut lock(&state), (turnable, unturned(turnable)), None);
                    continue;
                }
                Read::Scheduled { schedule, view, listed, turnable } => (schedule, *view, listed, turnable),
            };
            if view.number < self.floor {
                debug!(
                    block = view.number,
                    floor = self.floor,
                    "the latest agreed block is older than the promotion's"
                );
                self.unread(applied.as_ref(), (&mut stamps, &mut confirmed, &mut holders), &mut episodes).await;
                continue;
            }
            episodes.unscheduled = false;
            last = Some((view.time, view.rule.interval));
            self.vault_signer.send_replace(Some(VaultSigner { epoch: view.epoch, address: view.signer }));
            // Before any exemption: a vault naming another key ends this run whether the rule applies.
            self.named(&state, view.signer);
            let Some((named, listed)) = view.named.filter(|_| view.turn_names_operator()).zip(listed) else {
                applied = None;
                self.exempt(&mut lock(&state), (turnable, unturned(turnable)), None);
                continue;
            };
            // The members' age counts from when they were read, not from each reuse.
            let read_at = match (&set, cached_set == Some((schedule, named.set, named.turn))) {
                (Some((_, at, _)), true) => *at,
                _ => tokio::time::Instant::now(),
            };
            set = Some(((schedule, named.set, named.turn), read_at, listed.clone()));
            // Every other member, the turn's named one included: past a boundary that is the successor.
            let others: Vec<Member> = listed.into_iter().filter(|member| member.admin_key != self.admin).collect();
            if others.is_empty() {
                // No member can be promoted before the boundary, nor past it before `E`: slots may be
                // witnessed already, so the next turn's count can pass from the later of the two.
                applied = None;
                let boundary = next_boundary(view.time);
                self.exempt(&mut lock(&state), (boundary, boundary.max(view.due_at)), Some((view.epoch, named.set)));
                continue;
            }
            let scope = (view.epoch, schedule, named.set);
            {
                let mut fencing = lock(&state);
                // The rule starts to apply: nothing is confirmed until a member answers, and the start
                // check runs again, so no checkpoint is signed and no command admitted past this point
                // until a member's status shows it holds nothing this log lacks.
                if fencing.applies != Some(true) {
                    self.gate.checked(false);
                    self.gate.confirm(0);
                    self.gate.lag(true);
                    self.gate.bound_stamps(0);
                }
                // Another set or schedule: no earlier member's confirmation counts until one of this
                // set's members answers.
                if applied.as_ref().is_some_and(|(_, was, _)| *was != scope) {
                    self.gate.confirm(0);
                }
                fencing.applies = Some(true);
                // The set's confirmations let checkpoints pass only until the next boundary, unless a
                // read at or past it names that set again.
                self.gate.holds_until(next_boundary(view.time).saturating_mul(1000));
                fencing.fence.read(view.due_at, view.signer, self.key);
                self.deadline(&fencing.fence);
                fencing.refused = refused(view.rule, self.tick);
                if let Some(reason) = fencing.refused
                    && !std::mem::replace(&mut episodes.refused, true)
                {
                    error!(
                        alert = "liveness_parameters_refused",
                        "refusing to sequence under the heartbeat rule: {reason}"
                    );
                }
            }
            let current = applied.insert((view, scope, others));
            self.round(current, (&mut stamps, &mut confirmed, &mut holders), &mut episodes).await;
        }
    }

    /// Latches a vault naming `signer` other than this enclave's key: the fence holds for good, and the
    /// node's run ends.
    fn named(&self, state: &Mutex<FenceState>, signer: AccountId) {
        let mut fencing = lock(state);
        fencing.fence.named(signer, self.key);
        if fencing.fence.replaced() {
            self.gate.replace();
            self.deadline(&fencing.fence);
        }
    }

    /// Records `fence`'s `E` in the gate, which every check reads against trusted time itself: the
    /// fence tick only alerts and opens it again.
    fn deadline(&self, fence: &Fence) {
        let due = if fence.replaced() { Some(0) } else { fence.due_ms() };
        self.gate.deadline(due, &self.trusted);
    }

    /// A round under the last read the rule applied under, `applied`, when this poll's read failed or
    /// came from an older block than the promotion's; nothing while the rule did not apply.
    async fn unread(&self, applied: Option<&Applied>, rounds: Rounds<'_>, episodes: &mut Episodes) {
        if let Some(applied) = applied {
            self.round(applied, rounds, episodes).await;
        }
    }

    /// Asks the members of `applied` for their statuses and takes them, then, while a checkpoint waits
    /// for a confirmation with nothing stamped, asks again at once rather than at the next poll, until
    /// one covers it or a poll's time has passed.
    async fn round(
        &self,
        (view, scope, others): &Applied,
        (stamps, confirmed, holders): Rounds<'_>,
        episodes: &mut Episodes,
    ) {
        let assessment = self.ask((view.epoch, scope.1), others.clone(), holders).await;
        self.take(&assessment, (view, *scope), stamps, confirmed, episodes);
        let hurried = tokio::time::Instant::now();
        while self.gate.waiting().is_some() && hurried.elapsed() < self.poll {
            tokio::time::sleep(HURRY).await;
            let assessment = self.ask((view.epoch, scope.1), others.clone(), holders).await;
            self.take(&assessment, (view, *scope), stamps, confirmed, episodes);
        }
    }

    /// Takes one round of statuses: the start check while it has not passed, the newest confirmation
    /// and the head the poster may sign at it, and the lag stop.
    fn take(
        &self,
        assessment: &Assessment,
        (view, scope): (&View, Scope),
        stamps: &mut Stamps,
        confirmed: &mut Option<(Scope, Signable)>,
        episodes: &mut Episodes,
    ) {
        let tip = *self.released.borrow();
        // Every head the sequencer released since the last round, sampled as it came.
        let stamps = stamps.with(tip);
        // An empty log holds nothing to confirm: at genesis it starts unconfirmed once a member answers,
        // an empty copy's included, and its first commands are what the members then confirm.
        let genesis = tip.head.link == B256::ZERO;
        if let Some((seq, member)) = assessment.conflict {
            error!(alert = "liveness_member_conflict", seq, %member,
                "a member's copy holds another link than this log at seq {seq}: a second history on this key");
        }
        if !self.gate.start_checked() {
            self.start_check(assessment, tip.head.seq, genesis, episodes);
        }
        // A member's copy matching this log at its durable seq holds every earlier seq too: the head is
        // signed at the newest released tip at or below it, whose stamp this rule kept. It signs nothing
        // from the read's next boundary, from which another set may apply, until a round under a read
        // past it gives it that read's.
        let until = next_boundary(view.time).saturating_mul(1000);
        if let Some((seq, member)) = assessment.confirmed {
            self.gate.confirm(seq);
            if let Some((_, released)) = stamps.range(..=seq).next_back()
                && confirmed.is_none_or(|(was, at)| was != scope || at.released.head.seq < released.head.seq)
            {
                let confirmer = Address::from(member.0);
                let signable = Signable { released: *released, confirmer, scope: Some((scope.0, scope.2)), until };
                *confirmed = Some((scope, signable));
            }
        }
        let current = confirmed.filter(|(was, _)| *was == scope).map(|(_, signable)| signable);
        // Until the start check passes the node stays stopped, heads included, so a promotion can pass it.
        let signing = current
            .filter(|_| self.gate.start_checked())
            .map_or(Signing::Nothing, |signable| Signing::Confirmed(Signable { until, ..signable }));
        self.signing.send_if_modified(|was| std::mem::replace(was, signing) != signing);
        // The log runs unconfirmed from the first command past the newest confirmed seq, as the earliest
        // released head past it shows (within a poll): with none yet, the next command starts that run
        // whenever it comes, however long the log was quiet before (a promotion's start, a restart).
        let since = current.filter(|signable| signable.released.head.seq < tip.head.seq).map(|signable| {
            let after = stamps.range(signable.released.head.seq.saturating_add(1)..).next();
            after.map_or(tip.stamp, |(_, released)| released.stamp)
        });
        let lagging = !genesis && lagging(tip, current, since, view.rule);
        self.gate.lag(lagging);
        let interval_ms = view.rule.interval.saturating_mul(1000);
        // With nothing past the confirmed seq, or at genesis, the next command may come at any time
        // within an interval of this round: the bound lapses on its own if the reads that would renew
        // it fail.
        let renewed = || {
            let now = self.trusted.reached_ms().and_then(|now| u64::try_from(now).ok());
            now.map_or(0, |now| now.saturating_add(interval_ms))
        };
        let bound = match (current, since) {
            _ if genesis => renewed(),
            (None, _) => 0,
            (Some(_), None) => renewed(),
            (Some(_), Some(since)) => since.saturating_add(interval_ms),
        };
        self.gate.bound_stamps(bound);
        match (lagging, std::mem::replace(&mut episodes.lagging, lagging)) {
            (true, false) => error!(
                alert = "liveness_unconfirmed",
                "no member has confirmed this log within a heartbeat interval of the first command past the \
                 newest confirmation, its tip at seq {}: commands stop until one does (newest confirmed {:?})",
                tip.head.seq,
                current.map(|signable| signable.released.head.seq)
            ),
            (false, true) => {
                info!(alert_resolved = "liveness_unconfirmed", "a member confirmed the log: commands resume")
            }
            _ => {}
        }
    }

    /// A read under which no promotion can pass before `until` seconds, and no other set can apply
    /// before `holds`, the next boundary a turn could start at: every part of the gate opens but the
    /// fence, which holds off until `until`, checkpoints pass unconfirmed only until `holds`, and the
    /// poster signs at the tip under `scope`, the read's signer epoch and set.
    fn exempt(&self, state: &mut FenceState, (holds, until): (u64, u64), scope: Option<(u64, u32)>) {
        state.applies = Some(false);
        state.fence.exempt(until);
        self.deadline(&state.fence);
        self.gate.exempt(holds.saturating_mul(1000));
        let tip = Signing::Tip(scope, holds.saturating_mul(1000));
        self.signing.send_if_modified(|signing| std::mem::replace(signing, tip) != tip);
    }

    /// Opens sequencing once a member holding a command answered, or at `genesis` any member, and none
    /// holds more than this log or a conflicting link; otherwise raises Critical `liveness_start_refused`
    /// naming the range, once.
    fn start_check(&self, assessment: &Assessment, tip: u64, genesis: bool, episodes: &mut Episodes) {
        let refusal = match (assessment.beyond, assessment.conflict) {
            (Some((seq, member)), _) => Some(format!(
                "member {member} holds seqs {}..={seq} this log does not: a rollback of this copy, or a gap to fetch",
                tip + 1
            )),
            (None, Some((seq, member))) => Some(format!("member {member} holds another link at seq {seq}")),
            (None, None) if assessment.answered == 0 && !(genesis && assessment.empty > 0) => None,
            (None, None) => {
                self.gate.checked(true);
                info!(answered = assessment.answered, tip, "the start check passed: sequencing");
                resolve(&mut episodes.start_refused, "liveness_start_refused", "the start check passed");
                return;
            }
        };
        if let Some(refusal) = refusal
            && !std::mem::replace(&mut episodes.start_refused, true)
        {
            error!(alert = "liveness_start_refused", "refusing to sequence: {refusal}");
        }
    }
}

mod assess;
pub(crate) use assess::assess;
mod attested;
mod episodes;
use episodes::{Episodes, resolve};
mod fencing;
pub(crate) mod limits;
use limits::{lagging, read_due, refused, unturned};
mod read;
use read::Read;
mod stamps;
use stamps::Stamps;
mod statuses;

#[cfg(test)]
mod tests;
