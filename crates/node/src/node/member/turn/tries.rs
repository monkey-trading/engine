//! A member's tries at its turn: what a failed tail handover pages, which failures rebuild the
//! replica, the stale copy's catch-up before its first take, and the copy set aside for a whole-log
//! take once the tries run out.

use anyhow::Context;
use tracing::{error, warn};

use super::STINT;
use crate::checkpoints::ScheduledTurn;
use crate::handover::tail::{CopyChanged, TailMiss, TailRefused};
use crate::standby::{Followed, Follower, ReplicaRefused};

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
