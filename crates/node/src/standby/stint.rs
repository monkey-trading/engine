//! What one follow stint of the standby says about its two follow alerts.

use super::chain::rollback_of;
use crate::log::Offset;

/// What one follow stint says about the two follow alerts.
#[derive(Debug, PartialEq)]
pub(super) enum StintAction {
    /// The stream itself failed: raise (once) `standby_follow_failed`; nothing else changes.
    FollowFailed,
    /// The stream worked and carried nothing new about a rollback: resolve `standby_follow_failed`
    /// if raised; no other change.
    Quiet,
    /// The stream worked and showed a rollback: resolve `standby_follow_failed` if raised; raise
    /// (once per episode) `operator_log_rollback` with the evidence.
    Rollback(String),
    /// The stream worked and repeated the whole tail of a rolled-back copy: resolve both alerts.
    Resolved,
}

/// The alert decision for one stint: the stream's failure is its own alert, whatever the failure
/// carries; an Ok stint that repeated the whole tail resolves a rollback episode, and a quiet Ok
/// stint resolves nothing but `standby_follow_failed`.
pub(super) fn stint_action(
    followed: &anyhow::Result<Option<Offset>>,
    rolled_back: bool,
    confirmed: bool,
) -> StintAction {
    match followed.as_ref().err().and_then(rollback_of) {
        None if followed.is_err() => StintAction::FollowFailed,
        Some(rollback) => StintAction::Rollback(rollback.to_string()),
        None if rolled_back && confirmed => StintAction::Resolved,
        None => StintAction::Quiet,
    }
}
