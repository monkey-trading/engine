//! The start's check of the posted log head against the recovered log, before anything is written.

use alloy::primitives::B256;
use anyhow::{Context, bail};
use tracing::error;

use crate::log_heads::Head;

/// Compares the posted head with the recovered log before anything is written. `link_at` returns
/// the link the replay captured at the posted seq from the bytes it authenticated, or `None`
/// where the seq sits at or under an attested snapshot base: the snapshot determines the content
/// there, and so the link, without the replay reaching it.
/// Error cases:
/// - the log ends before the posted head, the replay holds no command at the posted seq, or the
///   posted link differs at the posted seq
pub(crate) fn check_rollback(
    posted: Head,
    last_seq: u64,
    link_at: impl FnOnce(u64) -> anyhow::Result<Option<B256>>,
) -> anyhow::Result<()> {
    if posted.seq > last_seq {
        error!(
            alert = "log_rollback",
            posted_seq = posted.seq,
            log_seq = last_seq,
            "the on-chain log head names seqs {}..={} this log does not hold: refusing to start",
            last_seq + 1,
            posted.seq
        );
        bail!("log lacks the posted head's seqs {}..={}", last_seq + 1, posted.seq);
    }
    if let Some(link) = link_at(posted.seq).context("locating the posted head in the log")?
        && link != posted.link
    {
        error!(
            alert = "log_rollback",
            seq = posted.seq,
            posted_link = %posted.link,
            log_link = %link,
            "the on-chain log head's link differs at seq {}: refusing to start",
            posted.seq
        );
        bail!("the log's link at the posted head's seq {} differs", posted.seq);
    }
    Ok(())
}
