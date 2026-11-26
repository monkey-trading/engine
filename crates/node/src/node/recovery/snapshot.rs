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
            let bounds = Bounds { finalized: parent.seq, before: None, keep_from: None };
            let roots = VaultRoots { vault: chain, at };
            snapshot_start_against(None, (snapshots, log), (domain, &signers), bounds, &roots).await
        }
        None => {
            let signers = BTreeSet::from([AccountId(key.address())]);
            let bounds = Bounds { finalized: u64::MAX, before: None, keep_from: None };
            snapshot_start_against(None, (snapshots, log), (domain, &signers), bounds, &NoRoots).await
        }
    }
}
