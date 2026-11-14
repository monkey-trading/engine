//! The operator's side of the liveness rule. Every `POLL` it reads the chain at the latest agreed
//! block and, while a turn names the operator and its set has another member:
//! - asks each other member for its status, over a fresh nonce, and takes a status that recovers to
//!   the member's promotable standby key, names this signer epoch, and holds a link this log holds at
//!   that seq, as that member's confirmation;
//! - lets the poster sign at the newest confirmed seq only (`Signing::Confirmed`);
//! - holds sequencing until a start check passes: a member answered, and none holds a seq past this
//!   log or a link that conflicts with it, which raises Critical `liveness_start_refused` naming the
//!   range;
//! - stops taking commands while the log runs more than one heartbeat interval of sequenced time past
//!   the newest confirmation (Critical `liveness_unconfirmed`);
//! - feeds the fence the read's `promotionDueAt`, which a separate tick checks against trusted time.
//!
//! While no turn names the operator, or its set has no other member, the gate is open and the poster
//! signs at the log's tip. A missing confirmation is never a hold: the heartbeat simply stops.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use alloy::primitives::{Address, B256};
use engine_core::types::AccountId;
use futures_util::future::{BoxFuture, join_all};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, error, info, warn};

use crate::liveness::chain::{LivenessDomain, Member, Rule, members, view};
use crate::liveness::fence::Fence;
use crate::liveness::link::{Asking, Request, verified_status};
use crate::liveness::{DELTA, Gate, HEAD_ROUND_TRIP, MAX_SLOW_PPM, Released, Signable, Status, VaultSigner};
use crate::log_heads::Head;
use crate::sequencer::Request as SequencerRequest;
use crate::standby_key::chain::StandbyChain;
use crate::trusted_time::TrustedClock;

/// How often the operator reads the chain and asks the members.
const POLL: Duration = if cfg!(test) { Duration::from_millis(50) } else { Duration::from_secs(1) };
/// How often the fence is checked against trusted time.
const FENCE_TICK: Duration = if cfg!(test) { Duration::from_millis(10) } else { Duration::from_millis(250) };
/// How long a member gets to answer a status question.
const STATUS_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a set's members are read again.
const MEMBERS_EVERY: Duration = Duration::from_secs(60);
/// How many released heads' stamps the operator keeps for the confirmations it matches.
const STAMPS: usize = 16_384;

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

/// What the fence tick reads: whether the rule applies, the fence, and refused parameters.
#[derive(Clone, Copy, Debug, Default)]
struct FenceState {
    /// `None` before the first read; `Some(false)` while the rule does not apply.
    applies: Option<bool>,
    fence: Fence,
    /// Why the rule's parameters are refused, if they are.
    refused: Option<&'static str>,
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

/// The operator's liveness rule.
pub(crate) struct Operator<C, A> {
    pub chain: C,
    pub asking: A,
    pub domain: LivenessDomain,
    /// This enclave's address, which the vault must name.
    pub key: AccountId,
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
}

impl<C: StandbyChain, A: Asking> Operator<C, A> {
    /// Runs the rule until the released-head watch closes.
    /// Error cases:
    /// - none: failed reads and silent members are retried
    pub(crate) async fn run(mut self) -> anyhow::Result<()> {
        let state = Arc::new(Mutex::new(FenceState::default()));
        let fencing = tokio::spawn(fence_tick(state.clone(), self.gate.clone(), self.trusted.clone()));
        let _abort = AbortOnDrop(fencing);
        let mut poll = interval(POLL);
        poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut stamps: BTreeMap<u64, Released> = BTreeMap::new();
        let mut set: Option<(u32, tokio::time::Instant, Vec<Member>)> = None;
        let mut confirmed: Option<(u64, Signable)> = None;
        let mut episodes = Episodes::default();
        loop {
            poll.tick().await;
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
            let read = self.read(cached);
            let (view, listed) = match read.await {
                Ok(read) => {
                    self.vault_signer.send_replace(Some(VaultSigner { epoch: read.0.epoch, address: read.0.signer }));
                    resolve(&mut episodes.read_failed, "liveness_read_failed", "reading the chain works again");
                    read
                }
                Err(e) => {
                    if !std::mem::replace(&mut episodes.read_failed, true) {
                        warn!(alert = "liveness_read_failed", "reading the liveness rule failed: {e:#}");
                    }
                    continue;
                }
            };
            let Some((named, listed)) = view.named.filter(|_| view.turn_names_operator()).zip(listed) else {
                lock(&state).applies = Some(false);
                self.gate.open();
                self.signing.send_if_modified(|signing| std::mem::replace(signing, Signing::Tip) != Signing::Tip);
                continue;
            };
            set = Some((named.set, tokio::time::Instant::now(), listed.clone()));
            let others: Vec<Member> = listed.into_iter().filter(|member| member.candidate != named.candidate).collect();
            if others.is_empty() {
                lock(&state).applies = Some(false);
                self.gate.open();
                self.signing.send_if_modified(|signing| std::mem::replace(signing, Signing::Tip) != Signing::Tip);
                continue;
            }
            {
                let mut fencing = lock(&state);
                fencing.applies = Some(true);
                fencing.fence.read(view.due_at, view.signer, self.key);
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
            let assessment = self.ask(view.epoch, others).await;
            if let Some((seq, member)) = assessment.conflict {
                error!(alert = "liveness_member_conflict", seq, %member,
                    "a member's copy holds another link than this log at seq {seq}: a second history on this key");
            }
            if !self.gate.start_checked() {
                self.start_check(&assessment, tip.head.seq, &mut episodes);
            }
            // A member's copy matching this log at its durable seq holds every earlier seq too: the
            // head is signed at the newest released tip at or below it, whose stamp this rule kept.
            if let Some((seq, member)) = assessment.confirmed {
                self.gate.confirm(seq);
                if let Some((_, released)) = stamps.range(..=seq).next_back()
                    && confirmed
                        .is_none_or(|(epoch, at)| epoch != view.epoch || at.released.head.seq < released.head.seq)
                {
                    confirmed =
                        Some((view.epoch, Signable { released: *released, confirmer: Address::from(member.0) }));
                }
            }
            let current = confirmed.filter(|(epoch, _)| *epoch == view.epoch).map(|(_, signable)| signable);
            let signing = current.map_or(Signing::Nothing, Signing::Confirmed);
            self.signing.send_if_modified(|was| std::mem::replace(was, signing) != signing);
            let lagging = lagging(tip, current, view.rule);
            self.gate.lag(lagging);
            match (lagging, std::mem::replace(&mut episodes.lagging, lagging)) {
                (true, false) => error!(
                    alert = "liveness_unconfirmed",
                    "no member has confirmed this log within a heartbeat interval of its tip at seq {}: commands \
                     stop until one does (newest confirmed {:?})",
                    tip.head.seq,
                    current.map(|signable| signable.released.head.seq)
                ),
                (false, true) => {
                    info!(alert_resolved = "liveness_unconfirmed", "a member confirmed the log: commands resume")
                }
                _ => {}
            }
        }
    }

    /// The view at the latest agreed block, and, while a turn names the operator, its set's members:
    /// `cached` when it lists that set, read at the block otherwise.
    /// Error cases:
    /// - a read fails
    fn read(
        &self,
        cached: Option<(u32, Vec<Member>)>,
    ) -> BoxFuture<'_, anyhow::Result<(crate::liveness::chain::View, Option<Vec<Member>>)>> {
        Box::pin(async move {
            let block = self.chain.latest().await?;
            let view = view(&block, &self.domain).await?;
            let listed = match (view.named.filter(|_| view.turn_names_operator()), cached) {
                (Some(named), Some((id, listed))) if id == named.set => Some(listed),
                (Some(named), _) => Some(members(&block, &self.domain.standby, named.set).await?),
                (None, _) => None,
            };
            Ok((view, listed))
        })
    }

    /// Asks each of `others` for its status under signer epoch `epoch`, and weighs the verified
    /// answers against this log.
    fn ask(&self, epoch: u64, others: Vec<Member>) -> BoxFuture<'_, Assessment> {
        Box::pin(async move {
            let mut nonce = [0u8; 32];
            if SystemRandom::new().fill(&mut nonce).is_err() {
                warn!("the RNG failed: no status questions this round");
                return Assessment::default();
            }
            let request = Request::Status { epoch, nonce: B256::from(nonce) };
            let mut asked = Vec::with_capacity(others.len());
            for member in &others {
                asked.push(self.status(member.clone(), request.clone(), nonce, epoch));
            }
            let statuses: Vec<(AccountId, Status)> = join_all(asked).await.into_iter().flatten().collect();
            let tip = self.released.borrow().head;
            let mut seqs: Vec<u64> = Vec::new();
            for (_, status) in &statuses {
                seqs.extend([Some(status.durable), status.seen].into_iter().flatten().map(|head| head.seq));
            }
            seqs.retain(|seq| *seq <= tip.seq);
            seqs.sort_unstable();
            seqs.dedup();
            let mut prefixes = BTreeMap::new();
            for seq in seqs {
                match self.prefix_at(seq).await {
                    Ok(prefix) => _ = prefixes.insert(seq, prefix),
                    Err(e) => debug!(seq, "no prefix to match a status against: {e:#}"),
                }
            }
            assess(&statuses, |seq| prefixes.get(&seq).copied().flatten(), tip)
        })
    }

    /// `member`'s status for `request`, once it recovers to the member's promotable standby key over
    /// `nonce` and names `epoch`; `None`, logged, otherwise.
    fn status(
        &self,
        member: Member,
        request: Request,
        nonce: [u8; 32],
        epoch: u64,
    ) -> BoxFuture<'_, Option<(AccountId, Status)>> {
        let deployment = (self.domain.standby.chain_id, self.domain.standby.vault);
        Box::pin(async move {
            let answered = async {
                let key = member.standby.ok_or_else(|| anyhow::anyhow!("no promotable standby key"))?;
                anyhow::ensure!(!member.endpoint.is_empty(), "no registered handover endpoint");
                let reply = tokio::time::timeout(STATUS_TIMEOUT, self.asking.ask(&member.endpoint, &request))
                    .await
                    .map_err(|_| anyhow::anyhow!("no answer within {STATUS_TIMEOUT:?}"))??;
                let status = verified_status(reply, deployment, member.candidate, key, &nonce)?;
                anyhow::ensure!(status.epoch == epoch, "the status names epoch {}", status.epoch);
                anyhow::Ok((member.candidate, status))
            };
            answered.await.inspect_err(|e| debug!(member = %member.candidate, "no status: {e:#}")).ok()
        })
    }

    /// This log's cumulative prefix after the command at `seq`, while the sequencer retains it.
    /// Error cases:
    /// - the sequencer has stopped
    async fn prefix_at(&self, seq: u64) -> anyhow::Result<Option<B256>> {
        let sequencer = self.sequencer.upgrade().ok_or_else(|| anyhow::anyhow!("the sequencer has stopped"))?;
        let (reply, answer) = oneshot::channel();
        sequencer
            .send(SequencerRequest::PrefixAt { seq, reply })
            .await
            .map_err(|_| anyhow::anyhow!("the sequencer has stopped"))?;
        answer.await.map_err(|_| anyhow::anyhow!("the sequencer dropped the question"))
    }

    /// Opens sequencing once a member answered and none holds more than this log or a conflicting
    /// link; otherwise raises Critical `liveness_start_refused` naming the range, once.
    fn start_check(&self, assessment: &Assessment, tip: u64, episodes: &mut Episodes) {
        let refusal = match (assessment.beyond, assessment.conflict) {
            (Some((seq, member)), _) => Some(format!(
                "member {member} holds seqs {}..={seq} this log does not: a rollback of this copy, or a gap to fetch",
                tip + 1
            )),
            (None, Some((seq, member))) => Some(format!("member {member} holds another link at seq {seq}")),
            (None, None) if assessment.answered == 0 => None,
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
}

/// Ends the episode `open` marks, logging `line`.
fn resolve(open: &mut bool, alert: &'static str, line: &str) {
    if std::mem::take(open) {
        info!(alert_resolved = alert, "{line}");
    }
}

/// Aborts the fence tick when the rule stops.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Weighs verified `statuses` against this log, whose cumulative prefix at a seq `prefix_at` gives
/// while it is retained, and whose tip is `tip`: a member's durable copy, and the newest head it has
/// seen, each past the tip, at a seq where this log holds another link, or matching.
pub(crate) fn assess(
    statuses: &[(AccountId, Status)],
    prefix_at: impl Fn(u64) -> Option<B256>,
    tip: Head,
) -> Assessment {
    let mut out = Assessment { answered: statuses.len(), ..Default::default() };
    for &(member, status) in statuses {
        for head in [Some(status.durable), status.seen].into_iter().flatten() {
            if head.seq > tip.seq {
                out.beyond = out.beyond.max(Some((head.seq, member)));
                continue;
            }
            match prefix_at(head.seq) {
                Some(link) if link != head.link => out.conflict = out.conflict.or(Some((head.seq, member))),
                Some(_) if head == status.durable => {
                    out.confirmed = out.confirmed.max(Some((head.seq, member)));
                }
                _ => {}
            }
        }
    }
    out
}

/// Whether the log at `tip` runs more than one heartbeat interval of sequenced time past `confirmed`,
/// or no seq is confirmed.
fn lagging(tip: Released, confirmed: Option<Signable>, rule: Rule) -> bool {
    let interval_ms = rule.interval.saturating_mul(1000);
    confirmed.is_none_or(|signable| tip.stamp.saturating_sub(signable.released.stamp) > interval_ms)
}

/// Why the node refuses to sequence under `rule` with a ticker every `tick`: a detection window at or
/// below `DELTA` plus twice the head round trip, or a ticker slower than half an interval, which would
/// leave a live head no newer seq to sign.
fn refused(rule: Rule, tick: Duration) -> Option<&'static str> {
    let window = Duration::from_secs(rule.interval.saturating_mul(u64::from(rule.misses)));
    if window <= DELTA + 2 * HEAD_ROUND_TRIP {
        return Some("heartbeatMisses x heartbeatInterval is at or below the stop margin plus two head round trips");
    }
    if tick > rule.interval() / 2 {
        return Some("the ticker interval is above half the heartbeat interval");
    }
    None
}

/// Checks the fence against trusted time every `FENCE_TICK`, closing the gate once the enclave's
/// upper bound on true time reaches `E - DELTA` and opening it on a later `E`, and alerting on each
/// change; a rule that does not apply leaves it open.
async fn fence_tick(state: Arc<Mutex<FenceState>>, gate: Gate, trusted: TrustedClock) {
    let mut tick = interval(FENCE_TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut fenced = false;
    loop {
        tick.tick().await;
        let FenceState { applies, fence, refused } = *lock(&state);
        let now = match applies {
            Some(false) => false,
            None => true,
            Some(true) => refused.is_some() || fence.fenced(trusted.upper_bound_ms(MAX_SLOW_PPM)),
        };
        gate.fence(now);
        match (now, std::mem::replace(&mut fenced, now)) {
            (true, false) if applies == Some(true) => error!(
                alert = "liveness_fenced",
                due_ms = ?fence.due_ms(),
                replaced = fence.replaced(),
                "the operator stops by its own clock: a promotion may pass within the stop margin; no command, \
                 answer or head is signed until a later read moves it on"
            ),
            (false, true) => info!(alert_resolved = "liveness_fenced", "a later read moved the earliest promotion on"),
            _ => {}
        }
    }
}

/// The fence state, whatever a panic left.
fn lock(state: &Mutex<FenceState>) -> std::sync::MutexGuard<'_, FenceState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
