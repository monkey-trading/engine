//! The alerts a member raises while it follows. Each pages once per episode, and its resolution
//! repeats the fields it paged with, so the router ends the episode it opened: a step of following
//! that keeps failing pages Critical `member_follow_blocked` once it has failed `PERSISTS` polls in a
//! row, and the stream's failures, a replica that refuses an entry, and evidence that the operator
//! rolled its log back page at once.

use std::collections::BTreeMap;

use tracing::{error, info, warn};

/// How many polls in a row a step must fail before it pages: one failure is a stint's hiccup, more is
/// an outage the member's next turn may not survive.
pub(super) const PERSISTS: u32 = 2;

/// A step of following whose failures page `member_follow_blocked`, each its own episode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Step {
    /// Reading the turn in effect at trusted time: the vault, the schedule, or a time sample.
    ReadTurn,
    /// Resolving the operator in effect.
    ResolveOperator,
    /// Opening the copy and building its replica, which replays it.
    OpenCopy,
    /// Reading the latest posted log head.
    ReadPostedHead,
    /// Pinning the latest posted log head, which the replica passed without keeping.
    PinPostedHead,
}

impl Step {
    /// The name the page's `step` field carries.
    fn as_str(self) -> &'static str {
        match self {
            Step::ReadTurn => "read_turn",
            Step::ResolveOperator => "resolve_operator",
            Step::OpenCopy => "open_copy",
            Step::ReadPostedHead => "read_posted_head",
            Step::PinPostedHead => "pin_posted_head",
        }
    }
}

/// The member's open episodes, kept across its waits so a resolution after a holding still ends one.
#[derive(Default)]
pub(crate) struct Episodes {
    /// Each failing step's failures in a row, and whether its episode paged.
    blocked: BTreeMap<Step, (u32, bool)>,
    /// The operator the open `standby_follow_failed` episode names.
    follow_failed: Option<String>,
    /// The operator the open `replica_diverged` episode names.
    diverged: Option<String>,
    /// The operator the open stream `operator_log_rollback` episode names.
    rolled_back: Option<String>,
    /// The seq and link of the posted head the open posted-head `operator_log_rollback` episode names.
    posted_rollback: Option<(u64, String)>,
}

impl Episodes {
    /// Counts a failure of `step`, which pages once it has failed `PERSISTS` polls in a row.
    pub(crate) fn blocked(&mut self, step: Step, e: &anyhow::Error) {
        let (failures, paged) = self.blocked.entry(step).or_default();
        *failures += 1;
        if *failures >= PERSISTS && !std::mem::replace(paged, true) {
            error!(
                alert = "member_follow_blocked",
                step = step.as_str(),
                "this member's following has failed {failures} polls in a row, and its next turn may not be \
                 taken: {e:#}"
            );
        } else {
            warn!(step = step.as_str(), "a step of this member's following failed: {e:#}");
        }
    }

    /// Ends `step`'s run of failures, and its episode if it paged.
    pub(crate) fn cleared(&mut self, step: Step) {
        if let Some((_, true)) = self.blocked.remove(&step) {
            info!(alert_resolved = "member_follow_blocked", step = step.as_str(), "this member follows again");
        }
    }

    /// Pages `standby_follow_failed` for `operator`, once per episode.
    pub(crate) fn follow_failed(&mut self, operator: &str, e: &anyhow::Error) {
        if self.follow_failed.is_none() {
            error!(alert = "standby_follow_failed", operator, "following failed: {e:#}");
            self.follow_failed = Some(operator.to_owned());
        }
    }

    /// Pages `replica_diverged` for `operator`, once per episode.
    pub(crate) fn diverged(&mut self, operator: &str, e: &anyhow::Error) {
        if self.diverged.is_none() {
            error!(
                alert = "replica_diverged",
                operator,
                "an entry the operator sealed does not replay on this member's replica, which the copy does not \
                 take: the replica is rebuilt from the copy: {e:#}"
            );
            self.diverged = Some(operator.to_owned());
        }
    }

    /// Pages `operator_log_rollback` for `operator`'s stream with `evidence`, once per episode.
    pub(crate) fn rolled_back(&mut self, operator: &str, evidence: &str) {
        if self.rolled_back.is_none() {
            error!(
                alert = "operator_log_rollback",
                operator, "{evidence}; the copy takes nothing more and is kept as the source to restore it from"
            );
            self.rolled_back = Some(operator.to_owned());
        }
    }

    /// A stint followed: ends the follow-failure, divergence and stream-rollback episodes.
    pub(crate) fn followed(&mut self) {
        if let Some(operator) = self.follow_failed.take() {
            info!(alert_resolved = "standby_follow_failed", operator, "following works again");
        }
        if let Some(operator) = self.diverged.take() {
            info!(alert_resolved = "replica_diverged", operator, "the replica takes the operator's entries again");
        }
        if let Some(operator) = self.rolled_back.take() {
            info!(
                alert_resolved = "operator_log_rollback",
                operator, "the operator's log holds the copy's history again"
            );
        }
    }

    /// Pages `operator_log_rollback` for the posted head at `seq` naming `link`, which the copy
    /// contradicts, once per head.
    pub(crate) fn posted_contradicts(&mut self, seq: u64, link: String) {
        if self.posted_rollback.as_ref().is_some_and(|(paged, _)| *paged == seq) {
            return;
        }
        self.posted_held();
        error!(
            alert = "operator_log_rollback",
            seq,
            posted_link = %link,
            "the operator's posted log head names another history than this member's copy holds at its seq"
        );
        self.posted_rollback = Some((seq, link));
    }

    /// The latest posted head is one the copy holds, or none is posted: ends the posted-head
    /// rollback episode.
    pub(crate) fn posted_held(&mut self) {
        if let Some((seq, link)) = self.posted_rollback.take() {
            info!(
                alert_resolved = "operator_log_rollback",
                seq,
                posted_link = %link,
                "the latest posted log head is one this member's copy holds"
            );
        }
    }
}
