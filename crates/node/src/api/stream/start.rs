//! Where a stream starts, and what refuses a start before it upgrades the connection.

use std::path::Path;

use tokio::task::spawn_blocking;

use crate::api::ApiError;
use crate::log::Offset;

/// The offset a stream starts at: after `after`, at the first message of seq `from`, or with the next
/// message when it gives neither.
/// Error cases:
/// - both are given
pub(super) fn offset(after: Option<Offset>, from: Option<u64>) -> Result<Option<Offset>, ApiError> {
    match (after, from) {
        (Some(_), Some(_)) => Err(ApiError::BadRequest("give after or from, not both".into())),
        (Some(after), None) => Ok(Some(after.next())),
        (None, Some(seq)) => Ok(Some(Offset::start_of(seq))),
        (None, None) => Ok(None),
    }
}

/// Refuses a backfill from `start`, a seq the segments on this host do not reach back to: the log's oldest
/// segments are in its public archive, which the answer names. A log that cannot be checked is left to the
/// feed's own read.
/// Error cases:
/// - the segment holding `start` is listed but not on this host (`NotLocal`)
pub(super) async fn ensure_held(dir: &Path, start: Option<Offset>) -> Result<(), ApiError> {
    let Some(start) = start else { return Ok(()) };
    let dir = dir.to_owned();
    match spawn_blocking(move || crate::log::ensure_local(&dir, start.seq)).await {
        Ok(Err(e)) if e.is::<crate::log::NotLocal>() => Err(ApiError::BadRequest(format!("{e:#}"))),
        _ => Ok(()),
    }
}
