//! Where a stream starts, and what refuses a start before it upgrades the connection.

use std::path::Path;

use tokio::sync::watch;
use tokio::task::spawn_blocking;

use crate::api::ApiError;
use crate::log::{Manifest, Offset};

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

/// Refuses a backfill from `start`, a seq the segments on this host do not reach back to, or an offset before the
/// first this node's start verified, as `manifest` publishes it (`Manifest::ensure_served`): the log's
/// oldest segments are in its public archive, which the answer names. A log that cannot be checked is left to
/// the feed's own read.
/// Error cases:
/// - `start` is before the first verified offset (`NotVerified`), or the segment holding it is listed but not on
///   this host (`NotLocal`)
pub(super) async fn ensure_held(
    dir: &Path,
    manifest: &watch::Receiver<Manifest>,
    start: Option<Offset>,
) -> Result<(), ApiError> {
    let Some(start) = start else { return Ok(()) };
    let served = manifest.borrow().ensure_served(start);
    served.map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let dir = dir.to_owned();
    match spawn_blocking(move || crate::log::ensure_local(&dir, start.seq)).await {
        Ok(Err(e)) if e.is::<crate::log::NotLocal>() => Err(ApiError::BadRequest(format!("{e:#}"))),
        _ => Ok(()),
    }
}
