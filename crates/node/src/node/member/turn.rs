//! A member's wait for its turn: what it holds at trusted time, following the operator in effect
//! between its holdings, and taking its turn with the tail past its copy's tip.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Context;
use engine_core::turns::TURN_MS;
use engine_core::types::{AccountId, Millis};
use tracing::{Instrument, debug, info, info_span, warn};

use crate::checkpoints::setup::EpochUnfinal;
use crate::checkpoints::{CheckpointChain, ScheduledTurn, View};
use crate::handover::tail::{Taken, take_tail};
use crate::history::HistoryPlan;
use crate::history::marks::Retentions;
use crate::history::marks::fold::MarkFold;
use crate::history::marks::follow::Following;
use crate::log::{CheckpointDomain, MANIFEST};
use crate::node::member::standby::{Kept, Standby};
use crate::node::member::{Member, PostedHead, Replica, Sampling, Turn};
use crate::node::ready::{Moved, Ready, Start};
use crate::standby::{Followed, Follower, ReplicaRefused};
use crate::stats::Stats;
use crate::tee::sealing::file_for;

/// How long one follow stint runs before the member checks its turn again.
const STINT: Duration = if cfg!(test) { Duration::from_secs(2) } else { Duration::from_secs(15) };
/// The shortest stint a member runs: a shorter one may end before the stream's first frame, which
/// reads as a failed stint, so with less left before its own turn's boundary it waits instead.
const LEAST_STINT: Duration = if cfg!(test) { Duration::from_millis(250) } else { Duration::from_secs(1) };
/// How long after a turn's boundary its member may take to serve, during which a member following it
/// expects failed stints and pages none: five minutes, or half a turn when that is shorter.
pub(crate) const COMING_UP: Millis = if TURN_MS / 2 < 300_000 { TURN_MS / 2 } else { 300_000 };
/// How long, connecting included, a take follows the incumbent to catch the copy up after the start's
/// reads, a stint at a time, before it asks for the tail.
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
    /// Follow the operator in effect, under the turn in effect, if any, until the boundary of this
    /// member's own next turn, once drawn.
    Follow(Option<ScheduledTurn>, Option<Millis>),
}

impl Holding {
    /// The turn and generation it takes, `None` when it takes none.
    fn taking(&self) -> Option<(u64, u64)> {
        match self {
            Self::Take(turn) => Some((turn.turn, turn.generation)),
            Self::Hold | Self::Follow(..) => None,
        }
    }
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
        // The provider history the node's charts need, fetched while the member follows: nothing of it waits
        // for the turn (#884).
        let following = crate::history::marks::follow::Following::start(self.cfg)
            .inspect_err(|e| warn!("the member's provider backfill does not start: {e:#}"))
            .ok()
            .flatten();
        let turn = self.follow_until_turn(&mut standby, following.as_ref()).await;
        let staged = match following {
            Some(following) => Some(following.finish().await),
            None => None,
        };
        let turn = match (turn, staged) {
            (Ok(Turn::Replica(start)), Some(staged)) => {
                start.replica.marks.borrow_mut().stage(staged);
                Ok(Turn::Replica(start))
            }
            (turn, _) => turn,
        };
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
    async fn follow_until_turn(
        &mut self,
        standby: &mut Option<Standby>,
        following: Option<&Following>,
    ) -> anyhow::Result<Turn> {
        let mut follower: Option<Opened> = None;
        let mut followed: Option<(Followed, Option<u64>)> = None;
        let mut tries = Tries::default();
        loop {
            if let (Some(following), Some((replica, ..))) = (following, &follower) {
                following.observe(replica.replica().engine());
            }
            match self.keep_standby(standby, &mut follower, followed.as_ref().map(|(operator, _)| operator)).await {
                Ok(Kept::Start(start)) => return Ok(Turn::Replica(start)),
                Ok(Kept::Wait) => {
                    tokio::time::sleep(RETRY_PAUSE).await;
                    continue;
                }
                Ok(Kept::Follow) => {}
                Err(e) if e.downcast_ref::<Stopping>().is_some() => return Ok(Turn::Stop),
                Err(e) => warn!("the promoted member did not start: {e:#}"),
            }
            let holding = self.holding().await;
            match &holding {
                Ok(holding) => {
                    self.episodes.cleared(Step::ReadTurn);
                    if let Some(taking) = holding.taking() {
                        tries.at(taking);
                    }
                }
                Err(e) => self.episodes.blocked(Step::ReadTurn, e),
            }
            let (turn, own_next) = match holding {
                Ok(Holding::Hold) => return Ok(Turn::Hold),
                Ok(Holding::Take(turn)) => {
                    // A member that has not followed yet builds its replica from the copy it holds,
                    // if any; with none, the start takes the turn as a node with no copy does.
                    let mut just_opened = false;
                    if follower.is_none() {
                        if !self.cfg.node.log_dir.join(MANIFEST).exists() {
                            info!(turn = turn.turn, "no copy of the log to take the tail onto");
                            return Ok(Turn::Hold);
                        }
                        match self.open(None).await {
                            Ok(opened) => follower = Some(opened),
                            Err(e) if e.downcast_ref::<Stopping>().is_some() => return Ok(Turn::Stop),
                            // Only a copy that does not replay is left to the start's whole-log take.
                            Err(e) if e.downcast_ref::<CopyRefused>().is_some() => {
                                warn!("the copy does not replay for this member's turn: {e:#}");
                                return Ok(Turn::Hold);
                            }
                            // A chain read that failed is retried, as a failed take is, for `RETRY_FOR`.
                            Err(e) => {
                                warn!("opening the copy for this member's turn failed: {e:#}");
                                if tries.failed(false) {
                                    return Ok(Turn::Hold);
                                }
                                tokio::time::sleep(RETRY_PAUSE).await;
                                continue;
                            }
                        }
                        just_opened = true;
                    }
                    // A copy just opened, or whose last stint failed, follows the incumbent again.
                    if followed.is_none() {
                        match self.incumbent(&turn).await {
                            Ok(operator) => followed = Some((operator, Some(turn.turn))),
                            Err(e) => debug!(turn = turn.turn, "no incumbent to follow while taking the turn: {e:#}"),
                        }
                    }
                    // A stale copy, restarted past its boundary or rebuilt, catches up with the incumbent
                    // first, within bounds, so its first take names a tip the incumbent still retains.
                    if just_opened
                        && let (Some((replica, ..)), Some((operator, _))) = (follower.as_mut(), followed.as_ref())
                    {
                        catch_up(self.cfg, replica, operator).await;
                    }
                    let (replica, ..) = follower.as_mut().expect("opened above");
                    info!(turn = turn.turn, "this member's turn has come: reading what its start needs, then the tail");
                    let operator = followed.as_ref().map(|(operator, _)| operator);
                    let span = info_span!("take", turn = turn.turn);
                    match self.take(&turn, replica, operator).instrument(span.clone()).await {
                        Ok((Taken { store, after, entries, bytes }, ready)) if self.cfg.snapshots.is_none() => {
                            info!(after, entries, bytes, generation = store.generation(), "the copy took the tail");
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
                        Err(e) if e.downcast_ref::<EpochUnfinal>().is_some() || behind(&e) => {
                            // A promotion just before the boundary, or a latest block short of it: the turn
                            // waits, as a turn drawn within a finality period does, its copy kept and followed,
                            // so the incumbent's confirmations go on.
                            let first = tries.waits();
                            if first {
                                span.in_scope(|| info!(turn = turn.turn, "waiting to take the turn: {e:#}"));
                            } else {
                                span.in_scope(|| debug!(turn = turn.turn, "still waiting to take the turn: {e:#}"));
                            }
                            let following = follower.as_mut().zip(followed.as_ref());
                            let failed = match following {
                                // A latest block short of the boundary most often catches up within seconds: the
                                // copy follows through the pause alone, so the take is retried as soon as before.
                                Some(((replica, ..), (operator, _))) if behind(&e) => {
                                    let pause = tokio::time::sleep(RETRY_PAUSE);
                                    replica.follow_during(self.cfg, operator, RETRY_PAUSE, pause).await.1.err()
                                }
                                Some(((replica, ..), (operator, _))) => {
                                    let stint = replica.stint(self.cfg, operator, STINT).await.err();
                                    tokio::time::sleep(RETRY_PAUSE).await;
                                    stint
                                }
                                None => {
                                    tokio::time::sleep(RETRY_PAUSE).await;
                                    None
                                }
                            };
                            if let Some(e) = failed {
                                // The finalised source may name an operator promoted past: a warning once a wait.
                                if first {
                                    warn!("following while waiting to take the turn failed: {e:#}");
                                } else {
                                    debug!("following while waiting to take the turn failed again: {e:#}");
                                }
                                if damaged(&e) {
                                    follower = None;
                                }
                            }
                            continue;
                        }
                        Err(e) => {
                            // A take that reached its sources paged each one's failure there.
                            if e.downcast_ref::<SourcesFailed>().is_none() {
                                span.in_scope(|| warn!(turn = turn.turn, "taking this member's turn failed: {e:#}"));
                            }
                            if tries.failed(refused(&e)) {
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
                Ok(Holding::Follow(turn, own_next)) => (turn, own_next),
                Err(_) => {
                    tokio::time::sleep(STINT).await;
                    continue;
                }
            };
            // The operator in effect changes at each turn's boundary.
            let number = turn.as_ref().map(|turn| turn.turn);
            if followed.as_ref().is_none_or(|(_, resolved_at)| *resolved_at != number) {
                match self.resolve_following(turn.as_ref()).await {
                    Ok(resolved) => {
                        info!(operator = %resolved.origin, enclave = %resolved.enclave, turn = ?number, "following the operator in effect");
                        self.episodes.cleared(Step::ResolveOperator);
                        followed = Some((resolved, number));
                    }
                    Err(e) => {
                        self.episodes.blocked(Step::ResolveOperator, &e.context("resolving the operator in effect"));
                        tokio::time::sleep(self.before_own_turn(own_next, STINT).await).await;
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
                        // As a failed stint: the next try resolves the other operator to bootstrap from.
                        (self.fallback.follow_holder, followed) = (!self.fallback.follow_holder, None);
                        tokio::time::sleep(self.before_own_turn(own_next, STINT).await).await;
                        continue;
                    }
                },
            };
            self.pin_posted(replica).await;
            let boundary = turn.as_ref().map(|turn| turn.boundary);
            // A member whose own turn is next follows only up to its boundary, then reads the turn.
            let budget = self.before_own_turn(own_next, STINT).await;
            if budget < LEAST_STINT {
                tokio::time::sleep(budget).await;
                continue;
            }
            match replica.stint(self.cfg, operator, budget).await {
                Ok(_) => {
                    self.episodes.followed(replica.confirmed());
                    if let Some(resolved) = self.moved_on(operator.enclave).await {
                        followed = Some((resolved, number));
                    }
                    tokio::time::sleep(self.before_own_turn(own_next, Duration::from_secs(1)).await).await;
                }
                Err(e) if e.downcast_ref::<ReplicaRefused>().is_some() => {
                    self.episodes.diverged(&operator.origin, &e);
                    follower = None;
                    tokio::time::sleep(self.before_own_turn(own_next, STINT).await).await;
                }
                Err(e) => {
                    let coming_up = self.coming_up(boundary).await;
                    match crate::standby::rollback_evidence(&e) {
                        Some(_) if Box::pin(self.set_aside_tail(replica, operator.enclave)).await => follower = None,
                        Some(evidence) => self.episodes.rolled_back(&operator.origin, &evidence),
                        // The turn's member serves only once it has taken the keys: a failure within
                        // `COMING_UP` of the boundary is expected, and pages only once that has passed.
                        None if coming_up => {
                            warn!(operator = %operator.origin, "the turn's member does not serve yet: {e:#}");
                        }
                        None => self.episodes.follow_failed(&operator.origin, &e),
                    }
                    // A failed stint re-resolves the operator in effect, the other of the one the
                    // registry names and the one whose head is newest (#596).
                    self.fallback.follow_holder = !self.fallback.follow_holder;
                    followed = None;
                    tokio::time::sleep(self.before_own_turn(own_next, STINT).await).await;
                }
            }
        }
    }

    /// Whether this member holds, takes, or follows: it holds once the keys are sealed for the turn in
    /// effect at trusted time that names its release, and takes that turn while they are not; with no
    /// turn in effect, or none drawn, it holds while the vault names its release, so the node starts as
    /// it always has, opening its keys or taking them by an election's handover. Anything else it
    /// follows, under the turn in effect, if any, until its own next turn's boundary when the latest
    /// drawn turn is that one.
    /// Error cases:
    /// - the vault or the schedule cannot be read, or no trusted time sample can be taken
    async fn holding(&mut self) -> anyhow::Result<Holding> {
        let at = self.vault.observe(View::Finalized, &[]).await?;
        let (turn, own_next) = match crate::handover::turns::latest_drawn(&self.vault, &at).await? {
            Some(latest) => {
                let reached = self.reached().await?;
                // The schedule draws a turn only within the turn before it, so a drawn turn whose boundary
                // trusted time has yet to reach is the next one.
                let own = latest.boundary > reached && latest.release == self.release.id();
                let turn = crate::handover::turns::turn_at(&self.vault, &at, Some(reached)).await?;
                (turn, own.then_some(latest.boundary))
            }
            None => (None, None),
        };
        let dir = &self.cfg.tee.sealing.as_ref().context("a member seals its keys")?.dir;
        let moving = self.signer_moving(&at).await?;
        if let Some(turn) = &turn
            && let Some(holding) = self.skipped(turn, own_next).await
        {
            return Ok(holding);
        }
        match turn {
            // The generation a confirmed handover gave away is the successor's now.
            Some(turn)
                if turn.release == self.release.id()
                    && self.handed.is_some_and(|handed| handed.generation == Some(turn.generation)) =>
            {
                Ok(Holding::Follow(Some(turn), own_next))
            }
            // A change of signer in flight moves past the keys of the turn the finalized view names.
            Some(turn) if turn.release == self.release.id() && moving => Ok(Holding::Follow(Some(turn), own_next)),
            Some(turn)
                if turn.release == self.release.id() && file_for(dir, &self.release, turn.generation).exists() =>
            {
                Ok(Holding::Hold)
            }
            Some(turn) if turn.release == self.release.id() => Ok(Holding::Take(turn)),
            Some(turn) => Ok(Holding::Follow(Some(turn), own_next)),
            None => {
                let current = self.vault.current_release(&at).await?;
                Ok(if current.id == self.release.id() && !moving {
                    Holding::Hold
                } else {
                    Holding::Follow(None, own_next)
                })
            }
        }
    }

    /// Whether trusted time is within `COMING_UP` of the `boundary` of the turn in effect, while its
    /// member takes the keys and comes up; a time that cannot be read is not.
    async fn coming_up(&mut self, boundary: Option<Millis>) -> bool {
        let Some(boundary) = boundary else { return false };
        self.reached().await.is_ok_and(|reached| reached < boundary.saturating_add(COMING_UP))
    }

    /// `wait`, cut short where trusted time reaches `own_next`, this member's own next turn's boundary,
    /// first; all of `wait` without one, or when trusted time cannot be read.
    async fn before_own_turn(&mut self, own_next: Option<Millis>, wait: Duration) -> Duration {
        let Some(boundary) = own_next else { return wait };
        let Ok(reached) = self.reached().await else { return wait };
        wait.min(Duration::from_millis(u64::try_from(boundary.saturating_sub(reached)).unwrap_or(0)))
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
}

mod episodes;
mod open;
mod skip;
pub(crate) use skip::Fallback;
use skip::behind;
mod tries;

pub(crate) use episodes::Episodes;
use episodes::Step;
pub(in crate::node::member) use tries::Stopping;
pub(crate) use tries::set_copy_aside;
use tries::{CopyRefused, SourcesFailed, Tries, catch_up, damaged, page_refusal, refused};

#[cfg(test)]
mod tests;
