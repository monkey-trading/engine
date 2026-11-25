//! The vault's finalized checkpoint, below which the archive's ranges are fixed: a takeover's first start cuts the
//! log back to it, a promotion sets aside a tail past the last posted head, and a start refuses a log it does not
//! hold, so nothing at or below it is ever rewritten. A range whose last seq is below it goes up; one past it waits.

use crate::checkpoints::{CheckpointChain, CheckpointRpc, View};

/// Where the uploader reads the vault's finalized checkpoint.
pub(crate) trait Finality {
    /// The seq of the vault's finalized checkpoint, `None` before its first.
    /// Error cases:
    /// - the vault cannot be read
    async fn finalized(&self) -> anyhow::Result<Option<u64>>;
}

impl Finality for CheckpointRpc {
    /// Reads the vault at finality through the checkpoint endpoint, as the checkpoint poster does.
    async fn finalized(&self) -> anyhow::Result<Option<u64>> {
        let at = self.observe(View::Finalized, &[]).await?;
        Ok(at.state.checkpoint.map(|(seq, _)| seq))
    }
}
