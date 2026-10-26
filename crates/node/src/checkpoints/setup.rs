//! Checkpoint setup before the node opens its log for writing.

use std::sync::Arc;

use anyhow::Context;
use engine_core::types::AccountId;
use tokio::sync::mpsc;

use crate::checkpoints::{ApprovalRegistry, CheckpointChain, CheckpointRpc, Observation, Poster, View, Work};
use crate::config::{EvmCustodySection, FinalityTag, NodeConfig};
use crate::log::CheckpointDomain;
use crate::tee::EnclaveKey;

pub(crate) struct Setup {
    pub poster: Poster<CheckpointRpc>,
    pub tx: mpsc::Sender<Work>,
    pub rx: mpsc::Receiver<Work>,
    pub approvals: ApprovalRegistry,
    pub observation: Observation,
    pub parent: crate::log::CheckpointParent,
    pub posted_seq: Option<u64>,
}

/// Checkpoint release follows custody withdrawal finality, independently of early credit depth.
fn release_policy(evm: &EvmCustodySection) -> (FinalityTag, u64) {
    (evm.withdrawal_finality_tag, 0)
}

/// Connects the checkpoint endpoint and reads its rollback anchor before log recovery.
/// Error cases:
/// - configuration, transport, or vault reads fail
pub(crate) async fn connect(cfg: &NodeConfig, key: &Arc<EnclaveKey>) -> anyhow::Result<Option<Setup>> {
    let Some(checkpoints) = &cfg.checkpoints else { return Ok(None) };
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let token = cfg.custody.evm.as_ref().context("checkpoint custody is not EVM")?.usdc;
    let evm = cfg.custody.evm.as_ref().context("checkpoint custody is not EVM")?;
    let (release_tag, release_depth) = release_policy(evm);
    let chain = CheckpointRpc::new(
        checkpoints,
        domain,
        token,
        release_tag,
        release_depth,
        &cfg.node.log_dir,
        AccountId(key.address()),
    )?;
    // A restart must not sign from a finalized parent while its own latest inclusion is pending.
    // Await that boundary without opening persisted state; an orphaned inclusion may disappear.
    let state = tokio::time::timeout(evm.finality_stalled_after, async {
        loop {
            let latest = chain.observe(View::Latest, &[]).await?;
            let finalized = chain.observe(View::Finalized, &[]).await?;
            latest.validate(&[])?;
            finalized.validate(&[])?;
            anyhow::ensure!(
                latest.state.signer == AccountId(key.address()) && finalized.state.signer == AccountId(key.address()),
                "vault signer differs from this enclave; signer recovery is required"
            );
            if latest.state.checkpoint == finalized.state.checkpoint
                && latest.lifecycle.as_ref().map(|evidence| evidence.signer_epoch)
                    == finalized.lifecycle.as_ref().map(|evidence| evidence.signer_epoch)
            {
                break anyhow::Ok(finalized);
            }
            tokio::time::sleep(checkpoints.retry_interval).await;
        }
    })
    .await
    .context("checkpoint startup awaits custody finality beyond its finality budget")??;
    chain.authenticate_reservation(&state).await?;
    let parent = match &state.state.checkpoint {
        Some((seq, root)) => crate::log::CheckpointParent { has_checkpoint: true, seq: *seq, root: root.parse()? },
        None => Default::default(),
    };
    let posted_seq = parent.has_checkpoint.then_some(parent.seq);
    let poster = Poster {
        chain,
        registry: Some(Default::default()),
        domain,
        signer: AccountId(key.address()),
        retry: checkpoints.retry_interval,
        pending_deposits_critical_after: evm.stall_critical_after,
    };
    let (tx, rx) = mpsc::channel(64);
    Ok(Some(Setup { poster, tx, rx, posted_seq, parent, approvals: Default::default(), observation: state }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_credit_and_its_confirmation_depth_cannot_weaken_checkpoint_release() {
        let cfg: NodeConfig = toml::from_str(include_str!("../../../../config/anvil.toml")).unwrap();
        let mut evm = cfg.custody.evm.unwrap();
        evm.withdrawal_finality_tag = FinalityTag::Finalized;
        for tag in [FinalityTag::Latest, FinalityTag::Safe, FinalityTag::Finalized] {
            evm.finality_tag = tag;
            evm.confirmations = 37;
            assert_eq!(release_policy(&evm), (FinalityTag::Finalized, 0));
        }
    }
}
