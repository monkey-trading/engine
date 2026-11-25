//! The vault's finalized checkpoint, below which the archive's ranges are fixed: a takeover's first start cuts the
//! log back to it, a promotion sets aside a tail past the last posted head, and a start refuses a log it does not
//! hold, so nothing at or below it is ever rewritten in the vault's history. A copy may still hold another history
//! past an earlier checkpoint, as a returning operator's does until its follow sets its superseded tail aside, so a
//! range goes up from a copy only below a finalized checkpoint the copy itself holds, at the vault's root.

use crate::checkpoints::{CheckpointChain, CheckpointRpc, View};

/// Where the uploader reads the vault's finalized checkpoint.
pub(crate) trait Finality {
    /// The seq and root of the vault's finalized checkpoint, `None` before its first.
    /// Error cases:
    /// - the vault cannot be read
    async fn finalized(&self) -> anyhow::Result<Option<(u64, String)>>;
}

impl Finality for CheckpointRpc {
    /// Reads the vault at finality through the checkpoint endpoint, as the checkpoint poster does.
    async fn finalized(&self) -> anyhow::Result<Option<(u64, String)>> {
        Ok(self.observe(View::Finalized, &[]).await?.state.checkpoint)
    }
}

/// Whether two spellings of a checkpoint's root name the same root.
pub(crate) fn same_root(a: &str, b: &str) -> bool {
    let parse = |root: &str| root.parse::<engine_core::eip712::B256>();
    matches!((parse(a), parse(b)), (Ok(a), Ok(b)) if a == b)
}
