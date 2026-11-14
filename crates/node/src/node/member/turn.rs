//! A member's wait for its turn: what it holds at trusted time, following the operator in effect
//! between its holdings, and taking its turn with the tail past its copy's tip.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Context;
use engine_core::types::Millis;
use tracing::{Instrument, info, info_span, warn};

use crate::checkpoints::{CheckpointChain, ScheduledTurn, View};
use crate::handover::tail::{Prepared, Taken, take_tail};
use crate::history::HistoryPlan;
use crate::history::marks::Retentions;
use crate::history::marks::fold::MarkFold;
use crate::log::{CheckpointDomain, MANIFEST};
use crate::node::member::standby::Standby;
use crate::node::member::{Member, PostedHead, Replica, Sampling, Turn};
use crate::node::ready::{Moved, Ready, Start};
use crate::standby::{Followed, Follower, ReplicaRefused};
use crate::stats::Stats;
use crate::tee::sealing::file_for;

/// How long one follow stint runs before the member checks its turn again.
const STINT: Duration = if cfg!(test) { Duration::from_secs(2) } else { Duration::from_secs(15) };
/// How long after a turn's boundary its member may take to serve, during which a member following it
/// expects failed stints and pages none.
pub(crate) const COMING_UP: Millis = 5 * 60_000;
/// How long the stint that catches the copy up after the start's reads runs before the tail.
const CATCH_UP: Duration = if cfg!(test) { Duration::from_millis(500) } else { Duration::from_secs(1) };
/// How long a trusted time sample bounds time from, before the member takes another.
const SAMPLE_FOR: Duration = if cfg!(test) { Duration::ZERO } else { Duration::from_secs(3600) };
/// How many takes its sources refuse (a tip they will not tail, or a tail that changed the copy)
/// before a member falls back to a whole-log take from the turn's sources, as a node with no copy
/// does. An older predecessor is no reason to: it refuses any request carrying `reads`, the
/// whole-log take's included.
const TAIL_ATTEMPTS: usize = 3;
/// How long a member retries takes that fail otherwise (the start's chain reads or time sample, or
/// no source answering) before it falls back to the start, whose own reads then raise the start's
/// alerts: a passing RPC or Roughtime outage does not discard the tail.
const RETRY_FOR: Duration = if cfg!(test) { Duration::from_secs(6) } else { Duration::from_secs(300) };
/// The least time between a member's tries at its turn.
const RETRY_PAUSE: Duration = if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(1) };

/// A member's copy opened with its replica, and what was built beside it: the dashboard record, the
/// history index's check, and the marks the replica folded.
pub(super) type Opened = (Follower, Stats, HistoryPlan, Rc<RefCell<MarkFold>>);

/// What a member does now.
enum Holding {
    /// Start the node as it always has.
    Hold,
    /// Take this turn's keys.
    Take(ScheduledTurn),
    /// Follow the operator in effect, under the turn in effect, if any.
    Follow(Option<ScheduledTurn>),
}

impl<C: CheckpointChain, H: PostedHead, T: Sampling> Member<'_, C, H, T> {
    /// Follows the operator in effect until this member holds a turn: the keys for the turn in effect
    /// are sealed here, or this member takes them with the tail past its copy's tip. With no turn in
    /// effect while the vault names this release, the node starts as it always has.
    /// After `TAIL_ATTEMPTS` failed tail handovers the start takes the turn as a node with no copy does.
    /// Error cases:
    /// - none past a configuration error: failed reads, stints and handovers are retried
    pub(crate) async fn until_turn(&mut self) -> anyhow::Result<Turn> {
        let mut standby = None;
        let turn = self.follow_until_turn(&mut standby).await;
        // The member link frees the handover endpoint before the node binds it.
        if let Some(standby) = standby {
            standby.stop().await;
        }
        turn
    }

    /// `until_turn`, with the member's standby side (`standby`) opened beside the following, and
    /// started as the operator when a promotion makes its standby key the vault's signer (`promoted`).
    /// Error cases:
    /// - as `until_turn`
    async fn follow_until_turn(&mut self, standby: &mut Option<Standby>) -> anyhow::Result<Turn> {
        let mut follower: Option<Opened> = None;
        let mut followed: Option<(Followed, Option<u64>)> = None;
        let (mut refusals, mut failing_since) = (0, None::<Instant>);
        loop {
            match self.keep_standby(standby, &mut follower, followed.as_ref().map(|(operator, _)| operator)).await {
                Ok(Some(start)) => return Ok(Turn::Replica(Box::new(start))),
                Ok(None) => {}
                Err(e) if e.downcast_ref::<Stopping>().is_some() => return Ok(Turn::Stop),
                Err(e) => warn!("the promoted member did not start: {e:#}"),
            }
            let holding = self.holding().await;
            match &holding {
                Ok(_) => self.episodes.cleared(Step::ReadTurn),
                Err(e) => self.episodes.blocked(Step::ReadTurn, e),
            }
            let turn = match holding {
                Ok(Holding::Hold) => return Ok(Turn::Hold),
                Ok(Holding::Take(turn)) => {
                    // A member that has not followed yet builds its replica from the copy it holds,
                    // if any; with none, the start takes the turn as a node with no copy does.
                    if follower.is_none() {
                        if !self.cfg.node.log_dir.join(MANIFEST).exists() {
                            info!(turn = turn.turn, "no copy of the log to take the tail onto");
                            return Ok(Turn::Hold);
                        }
                        match self.open(None).await {
                            Ok(opened) => follower = Some(opened),
                            Err(e) if e.downcast_ref::<Stopping>().is_some() => return Ok(Turn::Stop),
                            Err(e) => {
                                warn!("opening the copy for this member's turn failed: {e:#}");
                                return Ok(Turn::Hold);
                            }
                        }
                        // A stale copy, restarted past its boundary or rebuilt, catches up with the
                        // incumbent first, within bounds, so its first take names a tip the
                        // incumbent still retains.
                        if followed.is_none() {
                            followed = self.incumbent(&turn).await.ok().map(|operator| (operator, Some(turn.turn)));
                        }
                        if let (Some((replica, ..)), Some((operator, _))) = (follower.as_mut(), followed.as_ref()) {
                            catch_up(self.cfg, replica, operator).await;
                        }
                    }
                    let (replica, ..) = follower.as_mut().expect("opened above");
                    info!(turn = turn.turn, "this member's turn has come: reading what its start needs, then the tail");
                    let operator = followed.as_ref().map(|(operator, _)| operator);
                    let span = info_span!("take", turn = turn.turn);
                    match self.take(&turn, replica, operator).instrument(span.clone()).await {
                        Ok((Taken { store, after, entries, bytes }, ready)) if self.cfg.snapshots.is_none() => {
                            info!(after, entries, bytes, generation = store.generation(), "the copy took the tail");
                            revalidate(&turn)?;
                            let (follower, stats, history, marks) =
                                follower.take().expect("the follower took the tail");
                            let (replica, moved) =
                                (Replica { follower, stats, history, marks }, Moved { after, entries, bytes });
                            return Ok(Turn::Replica(Box::new(Start { replica, store, ready, moved })));
                        }
                        Ok((Taken { store, .. }, _)) => {
                            info!(generation = store.generation(), "the node recovers the log it took from disk");
                            return Ok(Turn::Hold);
                        }
                        Err(e) => {
                            // A take that reached its sources paged each one's failure there.
                            if e.downcast_ref::<SourcesFailed>().is_none() {
                                span.in_scope(|| warn!(turn = turn.turn, "taking this member's turn failed: {e:#}"));
                            }
                            if refused(&e) {
                                (refusals, failing_since) = (refusals + 1, None);
                            } else {
                                failing_since.get_or_insert_with(Instant::now);
                            }
                            let out_of_time = failing_since.is_some_and(|since| since.elapsed() >= RETRY_FOR);
                            if refusals >= TAIL_ATTEMPTS || out_of_time {
                                // The start takes the whole log, setting the copy aside first, as
                                // every whole-log take of a member's does (`before_whole_log_take`).
                                drop(follower.take());
                                return Ok(Turn::Hold);
                            }
                            // Only a copy the attempt may have damaged is rebuilt: any other keeps its
                            // replica, and its posted-head pin, current with a stint for the retry.
                            let kept = follower.as_mut().filter(|_| !damaged(&e)).zip(followed.as_ref());
                            match kept {
                                Some(((replica, ..), (operator, _))) => {
                                    if let Err(e) = replica.stint(self.cfg, operator, STINT).await {
                                        warn!("following before retrying this member's turn failed: {e:#}");
                                        if damaged(&e) {
                                            follower = None;
                                        }
                                    }
                                    // A stint the incumbent ends at once must not make retries a spin.
                                    tokio::time::sleep(RETRY_PAUSE).await;
                                }
                                None => {
                                    if damaged(&e) {
                                        follower = None;
                                    }
                                    tokio::time::sleep(STINT).await;
                                }
                            }
                            continue;
                        }
                    }
                }
                Ok(Holding::Follow(turn)) => turn,
                Err(_) => {
                    tokio::time::sleep(STINT).await;
                    continue;
                }
            };
            // The operator in effect changes at each turn's boundary.
            let number = turn.as_ref().map(|turn| turn.turn);
            if followed.as_ref().is_none_or(|(_, resolved_at)| *resolved_at != number) {
                match crate::standby::resolve(self.cfg, &self.vault).await {
                    Ok(resolved) => {
                        info!(operator = %resolved.origin, enclave = %resolved.enclave, turn = ?number, "following the operator in effect");
                        self.episodes.cleared(Step::ResolveOperator);
                        followed = Some((resolved, number));
                    }
                    Err(e) => {
                        self.episodes.blocked(Step::ResolveOperator, &e.context("resolving the operator in effect"));
                        tokio::time::sleep(STINT).await;
                        continue;
                    }
                }
            }
            let (operator, _) = followed.as_ref().expect("resolved above");
            let (replica, ..) = match follower.as_mut() {
                Some(opened) => opened,
                None => match self.open(Some(operator)).await {
                    Ok(opened) => {
                        self.episodes.cleared(Step::OpenCopy);
                        follower.insert(opened)
                    }
                    Err(e) if e.downcast_ref::<Stopping>().is_some() => return Ok(Turn::Stop),
                    Err(e) => {
                        self.episodes.blocked(Step::OpenCopy, &e.context("opening the follower's copy"));
                        tokio::time::sleep(STINT).await;
                        continue;
                    }
                },
            };
            self.pin_posted(replica).await;
            let boundary = turn.as_ref().map(|turn| turn.boundary);
            match replica.stint(self.cfg, operator, STINT).await {
                Ok(_) => {
                    self.episodes.followed();
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(e) if e.downcast_ref::<ReplicaRefused>().is_some() => {
                    self.episodes.diverged(&operator.origin, &e);
                    follower = None;
                    tokio::time::sleep(STINT).await;
                }
                Err(e) => {
                    let coming_up = self.coming_up(boundary).await;
                    match crate::standby::rollback_evidence(&e) {
                        Some(evidence) => self.episodes.rolled_back(&operator.origin, &evidence),
                        // The turn's member serves only once it has taken the keys: a failure within
                        // `COMING_UP` of the boundary is expected, and pages only once that has passed.
                        None if coming_up => {
                            warn!(operator = %operator.origin, "the turn's member does not serve yet: {e:#}");
                        }
                        None => self.episodes.follow_failed(&operator.origin, &e),
                    }
                    // A failed stint re-resolves the operator in effect.
                    followed = None;
                    tokio::time::sleep(STINT).await;
                }
            }
        }
    }

    /// Whether this member holds, takes, or follows: it holds once the keys are sealed for the turn in
    /// effect at trusted time that names its release, and takes that turn while they are not; with no
    /// turn in effect, or none drawn, it holds while the vault names its release, so the node starts as
    /// it always has, opening its keys or taking them by an election's handover. Anything else it
    /// follows, under the turn in effect, if any.
    /// Error cases:
    /// - the vault or the schedule cannot be read, or no trusted time sample can be taken
    async fn holding(&mut self) -> anyhow::Result<Holding> {
        let at = self.vault.observe(View::Finalized, &[]).await?;
        let turn = match crate::handover::turns::drawn(&self.vault, &at).await? {
            true => {
                let reached = self.reached().await?;
                crate::handover::turns::turn_at(&self.vault, &at, Some(reached)).await?
            }
            false => None,
        };
        let dir = &self.cfg.tee.sealing.as_ref().context("a member seals its keys")?.dir;
        match turn {
            // The generation a confirmed handover gave away is the successor's now.
            Some(turn)
                if turn.release == self.release.id()
                    && self.handed.is_some_and(|handed| handed.generation == Some(turn.generation)) =>
            {
                Ok(Holding::Follow(Some(turn)))
            }
            Some(turn)
                if turn.release == self.release.id() && file_for(dir, &self.release, turn.generation).exists() =>
            {
                Ok(Holding::Hold)
            }
            Some(turn) if turn.release == self.release.id() => Ok(Holding::Take(turn)),
            Some(turn) => Ok(Holding::Follow(Some(turn))),
            None => {
                let current = self.vault.current_release(&at).await?;
                Ok(if current.id == self.release.id() { Holding::Hold } else { Holding::Follow(None) })
            }
        }
    }

    /// Whether trusted time is within `COMING_UP` of the `boundary` of the turn in effect, while its
    /// member takes the keys and comes up; a time that cannot be read is not.
    async fn coming_up(&mut self, boundary: Option<Millis>) -> bool {
        let Some(boundary) = boundary else { return false };
        self.reached().await.is_ok_and(|reached| reached < boundary.saturating_add(COMING_UP))
    }

    /// The time true time has certainly reached: a trusted time sample's earliest bound and the
    /// monotonic time since it, sampling again once the last is `SAMPLE_FOR` old, and never below what
    /// it had reached when it last handed its keys over.
    /// Error cases:
    /// - no trusted time sample can be taken
    pub(super) async fn reached(&mut self) -> anyhow::Result<Millis> {
        if self.sample.is_none_or(|sample| sample.at.elapsed() > SAMPLE_FOR) {
            self.sample = Some(self.time.sample().await?);
        }
        let sample = self.sample.expect("sampled above");
        let reached =
            sample.earliest_ms.saturating_add(Millis::try_from(sample.at.elapsed().as_millis()).unwrap_or(Millis::MAX));
        Ok(reached.max(self.handed.and_then(|handed| handed.reached).unwrap_or(Millis::MIN)))
    }

    /// Opens the copy, bootstrapping an empty one from `operator`'s archive when given, and builds its
    /// replica by one replay, authenticated against every signer the vault recorded, with the
    /// dashboard record and the history index's check beside it, and the latest posted head pinned.
    /// Error cases:
    /// - the archive, the vault, the history index or the copy cannot be read, or the copy does not
    ///   replay
    pub(super) async fn open(&self, operator: Option<&Followed>) -> anyhow::Result<Opened> {
        let dir = self.cfg.node.log_dir.as_path();
        if let Some(operator) = operator.filter(|_| !dir.join(MANIFEST).exists()) {
            let archive = crate::standby::archive::archive_client(operator.danger_accept_invalid_certs)?;
            crate::standby::archive::bootstrap_archive(&archive, &operator.origins(), dir).await?;
        }
        let at = self.vault.observe(View::Finalized, &[]).await?;
        let signers = crate::handover::vault_signers(&self.vault, &at).await?;
        let enclave = operator.map_or(at.state.signer, |operator| operator.enclave);
        let domain = CheckpointDomain { chain_id: self.cfg.custody.chain_id, vault: self.cfg.custody.vault.parse()? };
        let (stats, mut history) = (Stats::new(self.cfg)?, HistoryPlan::for_member(&self.cfg.history)?);
        // The posted head is pinned as the replay passes it, so a rebuilt replica still holds it.
        let posted = match &self.heads {
            Some(heads) => heads.posted_head().await.ok().flatten().map(|head| (head.seq, head.link)),
            None => None,
        };
        // The marks the replica applies, folded for the cache this member cannot write while it follows.
        let marks = Rc::new(RefCell::new(MarkFold::new(Retentions::of(self.cfg.history.marks.as_ref())?)));
        let started = Instant::now();
        let segment_commands = self.cfg.node.segment_commands;
        let kept = (Some(stats.clone()), Some(marks.clone()));
        let stopping = &self.stopping;
        let follower = Follower::open_pinned(dir, segment_commands, domain, enclave, signers, kept, posted, |entry| {
            anyhow::ensure!(!stopping.load(Ordering::Acquire), Stopping);
            history.observe(entry);
            Ok(())
        })?;
        self.metrics.full_replays.inc();
        info!(seq = ?follower.replica().last_seq(), elapsed = ?started.elapsed(), "built the replica from the copy");
        Ok((follower, stats, history, marks))
    }

    /// Keeps the latest posted log head as one the replica holds, once it has passed it; a head the
    /// copy contradicts pages Critical `operator_log_rollback`, and one that cannot be read, or that
    /// the replica passed without keeping, pages `member_follow_blocked` once it persists.
    async fn pin_posted(&mut self, follower: &mut Follower) {
        let Some(heads) = &self.heads else { return };
        let head = match heads.posted_head().await {
            Ok(head) => head,
            Err(e) => return self.episodes.blocked(Step::ReadPostedHead, &e.context("reading the posted log head")),
        };
        self.episodes.cleared(Step::ReadPostedHead);
        let Some(head) = head else { return self.episodes.posted_held() };
        match follower.pin(head.seq, head.link) {
            Some(true) => {
                self.episodes.posted_held();
                self.episodes.cleared(Step::PinPostedHead);
            }
            Some(false) => self.episodes.posted_contradicts(head.seq, head.link.to_string()),
            None if follower.replica().last_seq().is_some_and(|last| last >= head.seq) => {
                let e =
                    anyhow::anyhow!("the replica passed the posted log head at seq {} without keeping it", head.seq);
                self.episodes.blocked(Step::PinPostedHead, &e);
            }
            None => {}
        }
    }

    /// Takes `turn` from the first of its sources that hands the keys over with the tail past the
    /// follower's tip, caught up once more from `operator`, the incumbent it followed, if any, after
    /// the start's reads. Everything the start reads from the chain is read before connecting, and the
    /// trusted time sample that admits its first stamps is taken last (`Ready`): from the confirmation
    /// on, nothing reads the chain or samples time.
    /// Error cases:
    /// - the reads or the sample fail, no source has a handover endpoint, or every source fails
    async fn take(
        &self,
        turn: &ScheduledTurn,
        follower: &mut Follower,
        operator: Option<&Followed>,
    ) -> anyhow::Result<(Taken, Ready)> {
        let at = self.vault.observe(View::Finalized, &[]).await?;
        let sources = crate::handover::turns::sources(&self.vault, &at, turn).await?;
        let mut prepared = Prepared::read(&self.vault, &self.release, Some(turn.boundary), None).await?;
        let read = Ready::read(
            self.cfg,
            &self.start,
            &self.metrics,
            prepared.signer,
            follower.replica(),
            (turn, prepared.generation),
            self.time.sample(),
        );
        let ready = read.await?;
        prepared.posted = ready.heads.and_then(|heads| heads.newest).map(|(_, head)| head);
        // The reads took time: one short stint catches the copy up to the incumbent, so the tail
        // stays short. A replica that refuses an entry must be rebuilt; any other failure leaves the
        // tail to cover the gap.
        if let Some(operator) = operator {
            match follower.stint(self.cfg, operator, CATCH_UP).await {
                Ok(_) => {}
                Err(e) if e.downcast_ref::<ReplicaRefused>().is_some() => return Err(e),
                Err(e) => warn!("catching the copy up before the tail failed: {e:#}"),
            }
        }
        // Every source's error is paged and classified as it comes, so an incumbent's refusal, or a
        // copy its tail changed, is not lost behind a later source's failure.
        let (mut failures, mut failed) = (Vec::new(), SourcesFailed::default());
        for source in sources {
            info!(turn = turn.turn, predecessor = %source.handover_endpoint, "taking this member's turn with the tail");
            let (sealer, attestor) = (self.sealer.as_ref(), self.attestor.as_ref());
            let endpoint = &source.handover_endpoint;
            match take_tail(self.cfg, endpoint, sealer, attestor, &prepared, follower).await {
                Ok(taken) => return Ok((taken, ready)),
                Err(e) => {
                    page_refusal(turn, &e);
                    failed.record(&e);
                    failures.push(format!("{endpoint}: {e:#}"));
                }
            }
        }
        let context = format!("no operator handed turn {}'s keys over: {}", turn.turn, failures.join("; "));
        Err(anyhow::Error::new(failed).context(context))
    }

    /// The operator `turn` takes over from, at `https://` the hostname its candidacy registered,
    /// against the vault's signer at finality: whom a stale copy catches up from.
    /// Error cases:
    /// - the turn names no source, or the vault cannot be read
    async fn incumbent(&self, turn: &ScheduledTurn) -> anyhow::Result<Followed> {
        let source = turn.source.as_ref().context("the turn names no source to follow")?;
        let at = self.vault.observe(View::Finalized, &[]).await?;
        let handover = self.cfg.tee.sealing.as_ref().and_then(|sealing| sealing.handover.as_ref());
        Ok(Followed {
            origin: format!("https://{}", source.api_hostname),
            enclave: at.state.signer,
            mirrors: handover.map(|handover| handover.mirrors.clone()).unwrap_or_default(),
            danger_accept_invalid_certs: handover.is_some_and(|handover| handover.danger_accept_invalid_certs),
        })
    }
}

/// A replica's build ended by a shutdown signal (`Member::wait`).
#[derive(Debug)]
pub(super) struct Stopping;

impl std::fmt::Display for Stopping {
    /// What happened.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the node is shutting down")
    }
}

impl std::error::Error for Stopping {}

/// The point between a confirmed tail handover and the first command the node sequences, where a
/// later rule binds the handover to the signer epoch, the turn, its assignment and its generation.
/// Nothing is checked here yet.
/// Error cases:
/// - none yet
fn revalidate(turn: &ScheduledTurn) -> anyhow::Result<()> {
    info!(turn = turn.turn, generation = turn.generation, "took the turn's keys and tail; starting from the replica");
    Ok(())
}

mod episodes;
mod tries;

pub(crate) use episodes::Episodes;
use episodes::Step;
pub(crate) use tries::set_copy_aside;
use tries::{SourcesFailed, catch_up, damaged, page_refusal, refused};

#[cfg(test)]
mod tests;
