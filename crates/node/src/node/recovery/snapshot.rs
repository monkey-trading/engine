//! The daily snapshot a node's own start takes, under `[daily_snapshots]`: the newest of those it holds whose
//! checkpoint its log holds, which the vault has finalized (with checkpoints), signed over its state and the
//! replay state it carries by the vault-recorded signer of its checkpoint, or, under mock custody, which has no
//! vault, by this node's own key, as a recovery cache is (`daily_snapshots::snapshot_start_against`).

use std::collections::BTreeSet;

use engine_core::types::AccountId;
use tracing::warn;

use crate::checkpoints::setup::Setup;
use crate::config::NodeConfig;
use crate::daily_snapshots::{Bounds, NoRoots, Signed, VaultRoots, snapshot_start_against};
use crate::log::CheckpointDomain;
use crate::tee::EnclaveKey;

/// The daily snapshot this start of `cfg`'s log takes, with `key`, under the vault `setup` reads, if any: none
/// without `[daily_snapshots]`, before the vault has finalized a checkpoint, or when none passes, each refusal
/// paged as Warning `snapshot_start_refused`, and the start replays the whole log.
pub(super) async fn chosen(cfg: &NodeConfig, key: &EnclaveKey, setup: Option<&Setup>) -> Option<Signed> {
    let snapshots = cfg.daily_snapshots.as_ref()?.dir.as_path();
    let vault = match cfg.custody.vault.parse() {
        Ok(vault) => vault,
        Err(e) => {
            warn!(alert = "snapshot_start_refused", "the configured vault does not parse: {e:#}");
            return None;
        }
    };
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault };
    let log = cfg.node.log_dir.as_path();
    let keep_from = framed(snapshots, &cfg.history.dir).map(|seq| seq.saturating_add(1));
    match setup {
        Some(setup) => {
            let parent = setup.parent.has_checkpoint.then_some(setup.parent)?;
            let (chain, at) = (&setup.poster.chain, &setup.observation);
            let signers = match crate::handover::vault_signers(chain, at).await {
                Ok(signers) => signers,
                Err(e) => {
                    warn!(alert = "snapshot_start_refused", "the vault's signers cannot be read: {e:#}");
                    return None;
                }
            };
            let bounds = Bounds { finalized: parent.seq, before: None, keep_from };
            let roots = VaultRoots { vault: chain, at };
            snapshot_start_against(None, (snapshots, log), (domain, &signers), bounds, &roots).await
        }
        None => {
            let signers = BTreeSet::from([AccountId(key.address())]);
            let bounds = Bounds { finalized: u64::MAX, before: None, keep_from };
            snapshot_start_against(None, (snapshots, log), (domain, &signers), bounds, &NoRoots).await
        }
    }
}

/// The seq of the newest daily snapshot in `snapshots` whose frozen marks copy this node holds in `history`, which a
/// start then goes on from no later than: one fetched from another node, as a member keeps them, has no copy here,
/// and a start from it would set the chart history before it aside. None when this node holds no copy.
fn framed(snapshots: &std::path::Path, history: &std::path::Path) -> Option<u64> {
    let listed = crate::daily_snapshots::read_manifest(snapshots).ok()?;
    let copied = |listing: &&crate::daily_snapshots::Listed| {
        let offset = crate::log::Offset { seq: listing.seq, index: listing.index };
        crate::history::marks::store::frozen_path(history, offset).exists()
    };
    listed.iter().rev().find(copied).map(|listing| listing.seq)
}
