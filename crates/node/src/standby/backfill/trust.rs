//! What a check of the standby's copy trusts, read from the chain once the copy is fetched: the keys it must
//! be signed and sealed by, and the daily snapshot it goes on from.

use std::collections::BTreeSet;
use std::path::Path;

use engine_core::types::AccountId;
use tracing::warn;

use crate::checkpoints::{CheckpointChain, View};
use crate::config::NodeConfig;
use crate::daily_snapshots::{Signed, open_signed};
use crate::handover::{Finalized, copy_start};
use crate::log::CheckpointDomain;
use crate::standby::operator::{Followed, copy_signers};

/// What a check of the copy trusts.
pub(in crate::standby) struct Trust {
    /// The keys every checkpoint, typed withdrawal and command seal must be one of (`operator::copy_signers`).
    pub signers: BTreeSet<AccountId>,
    /// The daily snapshot the check goes on from, whose checkpoint the copy holds; none for a check of the whole
    /// copy.
    pub start: Option<Signed>,
}

/// What a check of the copy in `dir` trusts, read from `chain` for `operator`: the signers, and the daily
/// snapshot under `snapshots`, `[daily_snapshots]`, that `copy_start` gives at or before the vault's finalized
/// checkpoint, by the signer the vault accepted at its seq or at that checkpoint (`Finalized::read`). A finalized
/// checkpoint that cannot be read is paged as Warning `snapshot_start_refused`, and the whole copy is checked.
/// Error cases:
/// - the signers cannot be read (`operator::copy_signers`)
pub(in crate::standby) async fn read_trust(
    cfg: &NodeConfig,
    chain: &impl CheckpointChain,
    operator: &Followed,
    (dir, snapshots): (&Path, Option<&Path>),
    domain: CheckpointDomain,
) -> anyhow::Result<Trust> {
    let signers = copy_signers(cfg, chain, operator).await?;
    let finalized = match snapshots {
        None => None,
        Some(_) => match chain.observe(View::Finalized, &[]).await {
            Ok(at) => Finalized::read(chain, &at, snapshots).await,
            Err(e) => {
                warn!(alert = "snapshot_start_refused", "the vault's finalized checkpoint cannot be read: {e:#}");
                None
            }
        },
    };
    let start = copy_start(snapshots, dir, (domain, &signers), finalized.as_ref()).await;
    Ok(Trust { signers, start })
}

/// The daily snapshot at `path`, which a replay already went on from, opened again for another replay, as
/// `copy_start` opened it; none, paged as Warning `snapshot_start_refused`, when it no longer opens.
pub(super) fn reopen(path: &Path, domain: CheckpointDomain, signers: &BTreeSet<AccountId>) -> Option<Signed> {
    open_signed(path, domain, signers)
        .inspect_err(|e| {
            warn!(alert = "snapshot_start_refused", "daily snapshot {} no longer opens: {e:#}", path.display())
        })
        .ok()
}
