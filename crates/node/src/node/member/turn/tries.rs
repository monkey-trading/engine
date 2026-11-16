//! A member's tries at its turn: what it counts toward leaving a holding to the start, what a failed
//! tail handover pages, which failures rebuild the replica, the stale copy's catch-up before its first
//! take, and the copy set aside for a whole-log take once the tries run out.

use std::time::Instant;

use anyhow::Context;
use engine_core::types::AccountId;
use tracing::{debug, error, info, warn};

use super::{RETRY_FOR, STINT, TAIL_ATTEMPTS};
use crate::checkpoints::{CheckpointChain, Observation, ScheduledTurn, View};
use crate::handover::tail::{CopyChanged, TailMiss, TailRefused};
use crate::node::member::turn::episodes::Step;
use crate::node::member::{Member, PostedHead, Sampling};
use crate::standby::{Followed, Follower, ReplicaRefused};

/// A member's tries at one holding, the turn and generation it takes: the sources' refusals, when its
/// other failures began since the last refusal, and whether it waits on the chain, which it logs once
/// per wait.
#[derive(Debug, Default)]
pub(super) struct Tries {
    pub(super) holding: Option<(u64, u64)>,
    pub(super) refusals: usize,
    pub(super) failing_since: Option<Instant>,
    waiting: bool,
}

impl Tries {
    /// Counts the tries at `holding`, the turn and generation the member takes now: afresh when it is
    /// another than the last taken, so an earlier take, one a skip moved the member off or an earlier
    /// turn's, carries no failures into it. Following between tries at one holding, as a failed read of
    /// the latest assignment reads, keeps them.
    pub(super) fn at(&mut self, holding: (u64, u64)) {
        if self.holding != Some(holding) {
            *self = Self { holding: Some(holding), ..Self::default() };
        }
    }

    /// Counts a failed take, `refused` by a source or failed otherwise: whether the member leaves the
    /// turn to the start, after `TAIL_ATTEMPTS` refusals, or failures for `RETRY_FOR` since the last.
    pub(super) fn failed(&mut self, refused: bool) -> bool {
        self.waiting = false;
        if refused {
            (self.refusals, self.failing_since) = (self.refusals + 1, None);
        } else {
            self.failing_since.get_or_insert_with(Instant::now);
        }
        self.refusals >= TAIL_ATTEMPTS || self.failing_since.is_some_and(|since| since.elapsed() >= RETRY_FOR)
    }

    /// Notes a take that waits on the chain, across which no failure counts: whether the wait is a new
    /// one, which the member logs.
    pub(super) fn waits(&mut self) -> bool {
        self.failing_since = None;
        !std::mem::replace(&mut self.waiting, true)
    }
}

/// Pages what one source's failed tail handover for `turn` says: a predecessor that did not find the
/// copy's tip on its log pages Critical `handover_tip_not_on_log`, one it found the copy lagging
/// behind, or its tail over the bound, Warning `handover_tail_lagged`; anything else is logged.
pub(super) fn page_refusal(turn: &ScheduledTurn, e: &anyhow::Error) {
    match e.downcast_ref::<TailRefused>().map(|refused| refused.miss) {
        Some(TailMiss::NotOnLog) => error!(
            alert = "handover_tip_not_on_log",
            turn = turn.turn,
            "the incumbent does not hold this copy's tip on its log, so it keeps the keys: the copy diverged \
             from the operator's log; after the retries this member takes the whole log aside its copy: {e:#}"
        ),
        Some(miss) => warn!(
            alert = "handover_tail_lagged",
            turn = turn.turn,
            cause = ?miss,
            "the incumbent will not tail this copy, which fell behind, so it keeps the keys while this member \
             catches up and retries: {e:#}"
        ),
        None => warn!(turn = turn.turn, "taking this member's turn with the tail failed: {e:#}"),
    }
}

/// What the sources of a failed take said, over every one it tried: whether any refused the copy's
/// tip before its stop, and whether any attempt may have changed the copy.
#[derive(Debug, Default)]
pub(super) struct SourcesFailed {
    pub refused: bool,
    pub damaged: bool,
}

impl SourcesFailed {
    /// Takes in one source's failure `e`.
    pub(super) fn record(&mut self, e: &anyhow::Error) {
        self.refused |= e.downcast_ref::<TailRefused>().is_some();
        self.damaged |= damaged(e);
    }
}

impl std::fmt::Display for SourcesFailed {
    /// What happened.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("every source failed")
    }
}

impl std::error::Error for SourcesFailed {}

/// The files of a copy that stay when it goes aside: the node's relayer journals, which belong to the
/// node, not to the log.
const KEPT_ASIDE: [&str; 2] = ["checkpoint-submission.json", "log-head-submission.json"];

/// Moves the copy in `dir` aside, to `<dir>.aside`, replacing any earlier one, so a whole-log take
/// starts from no copy and lands the holder's own segment files. With no copy in `dir`, nothing
/// moves, and an earlier aside stays.
/// Error cases:
/// - `dir` cannot be read, the earlier aside cannot be removed, or a file cannot be moved
pub(crate) fn set_copy_aside(dir: &std::path::Path) -> anyhow::Result<()> {
    let held = match std::fs::read_dir(dir) {
        Ok(entries) => entries.collect::<Result<Vec<_>, _>>()?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    if held.iter().all(|entry| KEPT_ASIDE.iter().any(|kept| entry.file_name() == *kept)) {
        return Ok(());
    }
    let aside = dir.with_extension("aside");
    if aside.exists() {
        std::fs::remove_dir_all(&aside).with_context(|| format!("removing {}", aside.display()))?;
    }
    std::fs::create_dir_all(&aside).with_context(|| format!("creating {}", aside.display()))?;
    for entry in held {
        if KEPT_ASIDE.iter().any(|kept| entry.file_name() == *kept) {
            continue;
        }
        std::fs::rename(entry.path(), aside.join(entry.file_name()))
            .with_context(|| format!("moving {} aside", entry.path().display()))?;
    }
    warn!(aside = %aside.display(), "moved this member's copy aside for a whole-log take");
    Ok(())
}

/// How far behind the incumbent's head, in commands, a copy counts as caught up before its first take.
const CAUGHT_UP: u64 = 4_096;
/// The most stints a stale copy follows for before its first take, caught up or not.
const CATCH_UP_STINTS: usize = 10;

/// Follows `operator` until `follower`'s copy is within `CAUGHT_UP` of its head, as its signed
/// heartbeats name it, for at most `CATCH_UP_STINTS` stints; a failed stint ends it.
pub(super) async fn catch_up(cfg: &crate::config::NodeConfig, follower: &mut Follower, operator: &Followed) {
    for _ in 0..CATCH_UP_STINTS {
        if let Err(e) = follower.stint(cfg, operator, STINT).await {
            warn!("catching the copy up before this member's turn failed: {e:#}");
            return;
        }
        if follower.behind().is_some_and(|behind| behind <= CAUGHT_UP) {
            return;
        }
    }
}

/// Whether a failed take was refused by a source: a tip it would not tail, or a tail that changed the
/// copy. Only these count toward `TAIL_ATTEMPTS`.
pub(super) fn refused(e: &anyhow::Error) -> bool {
    e.downcast_ref::<SourcesFailed>().is_some_and(|failed| failed.refused || failed.damaged)
}

/// Whether a failed attempt may have left the copy or its replica part-changed: the replica refused
/// an entry, or the handover failed once it began changing the copy. Only then is it rebuilt.
pub(super) fn damaged(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ReplicaRefused>().is_some()
        || e.downcast_ref::<CopyChanged>().is_some()
        || e.downcast_ref::<SourcesFailed>().is_some_and(|failed| failed.damaged)
}

/// A replica's build ended by a shutdown signal (`Member::wait`).
#[derive(Debug)]
pub(in crate::node::member) struct Stopping;

impl std::fmt::Display for Stopping {
    /// What happened.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the node is shutting down")
    }
}

impl std::error::Error for Stopping {}

impl<C: CheckpointChain, H: PostedHead, T: Sampling> Member<'_, C, H, T> {
    /// The operator in effect, when the vault names another enclave than `enclave`, the one followed:
    /// read after each stint, so a promotion moves the copy to the promoted key at once. `None` when
    /// it is the same one, or the read fails.
    pub(super) async fn moved_on(&self, enclave: AccountId) -> Option<Followed> {
        match crate::standby::resolve(self.cfg, &self.vault).await {
            Ok(resolved) if resolved.enclave != enclave => {
                info!(from = %enclave, to = %resolved.enclave, operator = %resolved.origin,
                    "the vault names another operator: following it");
                Some(resolved)
            }
            Ok(_) => None,
            Err(e) => {
                debug!("reading the operator in effect after a stint failed: {e:#}");
                None
            }
        }
    }

    /// Whether the vault's latest signer is another key than the one `at`, its finalized view, names:
    /// a change of signer in flight, as a promotion makes, whose keys the finalized turn no longer
    /// names once it is final, so no member reopens or takes that turn's keys meanwhile.
    /// Error cases:
    /// - the read fails
    pub(super) async fn signer_moving(&self, at: &Observation) -> anyhow::Result<bool> {
        Ok(self.vault.observe(View::Latest, &[]).await?.state.signer != at.state.signer)
    }

    /// Keeps the latest posted log head as one the replica holds, once it has passed it; a head the
    /// copy contradicts pages Critical `operator_log_rollback`, and one that cannot be read, or that
    /// the replica passed without keeping, pages `member_follow_blocked` once it persists.
    pub(super) async fn pin_posted(&mut self, follower: &mut Follower) {
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
}

/// A copy that did not replay when its replica was built: not a failed read, which is retried.
#[derive(Debug)]
pub(super) struct CopyRefused;

impl std::fmt::Display for CopyRefused {
    /// What happened.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the copy does not replay")
    }
}

impl std::error::Error for CopyRefused {}
