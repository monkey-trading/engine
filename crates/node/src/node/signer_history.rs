//! Historical signer identities are accepted only from the vault's finalized root history.

use std::path::Path;

use anyhow::ensure;

use crate::checkpoints::{APPROVAL_PAGE_SIZE, AcceptedCheckpoint, CheckpointChain, setup::Setup};
use crate::log::{LogEntry, SegmentedLog};

/// Loads immutable accepted identities without opening the log for append or repairing history.
/// Error cases:
/// - the log cannot be read, custody is unavailable, or an accepted identity differs from the log
pub(super) async fn load(setup: Option<&Setup>, dir: &Path) -> anyhow::Result<Vec<AcceptedCheckpoint>> {
    let Some(setup) = setup else { return Ok(Vec::new()) };
    let mut candidates = Vec::new();
    SegmentedLog::open(dir, |entry| {
        if let LogEntry::Checkpoint(checkpoint) = entry
            && setup.parent.has_checkpoint
            && checkpoint.seq <= setup.parent.seq
            && checkpoint.parent.is_some()
        {
            candidates.push(checkpoint);
        }
        Ok(())
    })?;
    let mut accepted = Vec::new();
    for page in candidates.chunks(APPROVAL_PAGE_SIZE) {
        let sequences = page.iter().map(|checkpoint| checkpoint.seq).collect::<Vec<_>>();
        let observed = setup.poster.chain.accepted_checkpoints(&setup.observation, &sequences).await?;
        ensure!(observed.len() == page.len(), "vault lacks accepted checkpoint signer history");
        for (authority, checkpoint) in observed.into_iter().zip(page) {
            authority.verify(checkpoint)?;
            accepted.push(authority);
        }
    }
    Ok(accepted)
}
