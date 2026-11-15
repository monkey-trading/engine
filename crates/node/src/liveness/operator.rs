//! The operator's side of the liveness rule. Every `POLL` it reads the chain at the latest agreed
//! block and, while a turn names the operator and its set has another member:
//! - asks each other member for its status, over a fresh nonce, and takes a status that recovers to
//!   the member's promotable standby key, names this signer epoch, and holds a link this log holds at
//!   that seq, as that member's confirmation;
//! - lets the poster sign at the newest confirmed seq only (`Signing::Confirmed`);
//! - holds sequencing until a start check passes: a member answered, or the log is empty at genesis,
//!   and none holds a seq past this log or a link that conflicts with it, which raises Critical
//!   `liveness_start_refused` naming the range;
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
//! signs at the log's tip, until the next turn boundary unless a later read moves it on. A missing
//! confirmation is never a hold: the heartbeat simply stops.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256};
use engine_core::types::AccountId;
use futures_util::future::BoxFuture;
use tokio::sync::{mpsc, watch};
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, error, info, warn};

use crate::liveness::chain::{LivenessDomain, Member, View, members, vault_signer, view};
use crate::liveness::fence::Fence;
use crate::liveness::link::Asking;
use crate::liveness::operator::fencing::{AbortOnDrop, FenceState, fence_tick, lock};
use crate::liveness::{Gate, Released, Signable, VaultSigner};
use crate::seed_relay::schedule::next_boundary;
use crate::sequencer::Request as SequencerRequest;
use crate::standby_key::chain::{AgreedBlock, StandbyChain, StandbyDomain};
use crate::trusted_time::TrustedClock;

/// How often the operator reads the chain and asks the members.
pub(crate) const POLL: Duration = Duration::from_secs(1);
/// How often the fence is checked against trusted time.
pub(super) const FENCE_TICK: Duration = if cfg!(test) { Duration::from_millis(10) } else { Duration::from_millis(250) };
/// How long a member gets to answer a status question.
const STATUS_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a set's members are read again.
const MEMBERS_EVERY: Duration = Duration::from_secs(60);
/// How many released heads' stamps the operator keeps for the confirmations it matches.
const STAMPS: usize = 16_384;
/// How soon the operator asks again while a checkpoint waits for a confirmation.
const HURRY: Duration = Duration::from_millis(25);

/// Where the poster may sign.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Signing {
    /// Nothing yet: no read has said whether the rule applies, or no member confirmed a head.
    #[default]
    Nothing,
    /// The log's tip: the rule does not apply, or the set has no other member.
    Tip,
    /// The newest seq a member confirmed.
    Confirmed(Signable),
}

/// What one round of statuses shows against this log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Assessment {
    /// How many members answered with a verified status.
    pub answered: usize,
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
type Rounds<'a> = (&'a mut BTreeMap<u64, Released>, &'a mut Option<(Scope, Signable)>, &'a mut Holders);

/// One read of the chain at the latest agreed block.
enum Read {
    /// The vault names no registry, or the registry no schedule, at a block of time `time`, where it
    /// names `signer`.
    Unscheduled { time: u64, signer: AccountId },
    /// The view under `schedule`, and, while a turn names the operator, its set's members.
    Scheduled { schedule: Address, view: Box<View>, listed: Option<Vec<Member>> },
}

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
    /// How often it reads the chain and asks the members: `POLL`.
    pub poll: Duration,
    /// For a promoted operator, the block its start read the promotion at: an older latest block, as a
    /// lagging provider majority serves, still names the outgoing key, so it counts as no read.
    pub floor: u64,
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
        let mut stamps: BTreeMap<u64, Released> = BTreeMap::new();
        let mut set: Option<(SetId, tokio::time::Instant, Vec<Member>)> = None;
        let mut confirmed: Option<(Scope, Signable)> = None;
        let mut applied: Option<Applied> = None;
        let mut holders = Holders::new();
        let mut episodes = Episodes::default();
        loop {
            // A checkpoint that waits for a confirmation wakes the rule at once.
            tokio::select! {
                _ = poll.tick() => {}
                () = self.gate.woken() => {}
            }
            if self.released.has_changed().is_err() {
                return Ok(());
            }
            let tip = *self.released.borrow_and_update();
            stamps.insert(tip.head.seq, tip);
            while stamps.len() > STAMPS {
                stamps.pop_first();
            }
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
            let (schedule, view, listed) = match read {
                Read::Unscheduled { time, signer } => {
                    if !std::mem::replace(&mut episodes.unscheduled, true) {
                        info!("the vault's registry names no schedule: the liveness rule does not apply");
                    }
                    self.named(&state, signer);
                    applied = None;
                    self.exempt(&mut lock(&state), unturned(time));
                    continue;
                }
                Read::Scheduled { schedule, view, listed } => (schedule, *view, listed),
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
            self.vault_signer.send_replace(Some(VaultSigner { epoch: view.epoch, address: view.signer }));
            // Before any exemption: a vault naming another key ends this run whether the rule applies.
            self.named(&state, view.signer);
            let Some((named, listed)) = view.named.filter(|_| view.turn_names_operator()).zip(listed) else {
                applied = None;
                self.exempt(&mut lock(&state), unturned(view.time));
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
                // Slots may be witnessed already: the next turn's count can pass at its boundary.
                applied = None;
                self.exempt(&mut lock(&state), next_boundary(view.time));
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
        while self.gate.wanted() > self.gate.confirmed() && hurried.elapsed() < self.poll {
            tokio::time::sleep(HURRY).await;
            let tip = *self.released.borrow();
            stamps.insert(tip.head.seq, tip);
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
        stamps: &BTreeMap<u64, Released>,
        confirmed: &mut Option<(Scope, Signable)>,
        episodes: &mut Episodes,
    ) {
        let tip = *self.released.borrow();
        // An empty log holds nothing a second copy could have extended past what the chain's posted
        // heads and checkpoints pin, and startup refuses a log they are not on: at genesis it starts
        // unconfirmed, and its first commands are what the members then confirm.
        let genesis = tip.head.link == B256::ZERO;
        if let Some((seq, member)) = assessment.conflict {
            error!(alert = "liveness_member_conflict", seq, %member,
                "a member's copy holds another link than this log at seq {seq}: a second history on this key");
        }
        if !self.gate.start_checked() {
            self.start_check(assessment, tip.head.seq, genesis, episodes);
        }
        // A member's copy matching this log at its durable seq holds every earlier seq too: the head is
        // signed at the newest released tip at or below it, whose stamp this rule kept.
        if let Some((seq, member)) = assessment.confirmed {
            self.gate.confirm(seq);
            if let Some((_, released)) = stamps.range(..=seq).next_back()
                && confirmed.is_none_or(|(was, at)| was != scope || at.released.head.seq < released.head.seq)
            {
                *confirmed = Some((scope, Signable { released: *released, confirmer: Address::from(member.0) }));
            }
        }
        let current = confirmed.filter(|(was, _)| *was == scope).map(|(_, signable)| signable);
        let signing = current.map_or(Signing::Nothing, Signing::Confirmed);
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

    /// A read under which no promotion can pass before `until` seconds: every part of the gate opens
    /// but the fence, which holds off until then, and the poster signs at the tip.
    fn exempt(&self, state: &mut FenceState, until: u64) {
        state.applies = Some(false);
        state.fence.exempt(until);
        self.deadline(&state.fence);
        self.gate.exempt();
        self.signing.send_if_modified(|signing| std::mem::replace(signing, Signing::Tip) != Signing::Tip);
    }

    /// The vault's registry and schedule at the latest agreed block, the view under them, and, while
    /// a turn names the operator, its set's members: `cached` when it lists that set of that schedule,
    /// read at the block otherwise.
    /// Error cases:
    /// - a read fails
    fn read(&self, cached: Option<(SetId, Vec<Member>)>) -> BoxFuture<'_, anyhow::Result<Read>> {
        Box::pin(async move {
            let block = self.chain.latest().await?;
            let Some(standby) = StandbyDomain::at_if_scheduled(&block, self.chain_id, self.vault).await? else {
                let signer = vault_signer(&block, self.vault).await?;
                return Ok(Read::Unscheduled { time: block.time(), signer });
            };
            let domain = LivenessDomain { standby, log_heads: self.log_heads };
            let view = view(&block, &domain).await?;
            let listed = match (view.named.filter(|_| view.turn_names_operator()), cached) {
                (Some(named), Some((id, listed))) if id == (standby.schedule, named.set, named.turn) => Some(listed),
                (Some(named), _) => Some(members(&block, &standby, (named.set, named.turn)).await?),
                (None, _) => None,
            };
            Ok(Read::Scheduled { schedule: standby.schedule, view: Box::new(view), listed })
        })
    }

    /// Opens sequencing once a member answered, or at `genesis` with no answer, and none holds more
    /// than this log or a conflicting link; otherwise raises Critical `liveness_start_refused` naming the
    /// range, once.
    fn start_check(&self, assessment: &Assessment, tip: u64, genesis: bool, episodes: &mut Episodes) {
        let refusal = match (assessment.beyond, assessment.conflict) {
            (Some((seq, member)), _) => Some(format!(
                "member {member} holds seqs {}..={seq} this log does not: a rollback of this copy, or a gap to fetch",
                tip + 1
            )),
            (None, Some((seq, member))) => Some(format!("member {member} holds another link at seq {seq}")),
            (None, None) if assessment.answered == 0 && !genesis => None,
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

/// The alert episodes the rule keeps open.
#[derive(Default)]
struct Episodes {
    read_failed: bool,
    refused: bool,
    start_refused: bool,
    lagging: bool,
    /// The last read found no schedule (logged, not an alert).
    unscheduled: bool,
}

/// Ends the episode `open` marks, logging `line`.
fn resolve(open: &mut bool, alert: &'static str, line: &str) {
    if std::mem::take(open) {
        info!(alert_resolved = alert, "{line}");
    }
}

mod assess;
pub(crate) use assess::assess;
mod fencing;
mod limits;
use limits::{lagging, refused, unturned};
mod statuses;

#[cfg(test)]
mod tests;
