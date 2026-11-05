//! The standby's takeover signal, read at one finalized block.

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, B256};
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};

use crate::checkpoints::rpc::{
    checkpointSignerAtCall, hasCheckpointCall, lastCheckpointAtCall, latestRootCall, latestSeqCall,
    operatorInEffectCall, parametersCall, pendingElectionCall, registryCall, rotationEnabledCall,
};
use crate::checkpoints::{CheckpointRpc, Takeover};

impl CheckpointRpc {
    /// Why a standby takes over: the registry named `candidate`'s key as the pending or current
    /// operator after the vault has had no checkpoint for the liveness timeout. Every read is
    /// pinned to one finalized block, so the decision to delete local history can never rest on
    /// a reorgable or self-inconsistent view. The signal names the vault's latest checkpoint —
    /// the anchor the published log must extend — and the signer the vault recorded there, whose
    /// later published checkpoints the copy keeps.
    /// Error cases:
    /// - an RPC call or decoding fails
    pub(crate) async fn takeover(&self, candidate: engine_core::types::AccountId) -> anyhow::Result<Option<Takeover>> {
        // One canonical block pins every read below, including the liveness clock.
        let block: serde_json::Value =
            self.deadline(self.client.request("eth_getBlockByNumber", (BlockNumberOrTag::Finalized, false))).await?;
        let hash: B256 = block
            .get("hash")
            .and_then(|value| value.as_str())
            .context("the finalized block has no hash")?
            .parse()
            .context("decoding the finalized block hash")?;
        let time = u64::from_str_radix(
            block.get("timestamp").and_then(|value| value.as_str()).context("no timestamp")?.trim_start_matches("0x"),
            16,
        )?;
        let at = BlockId::hash(hash);
        let vault = Address::from(self.domain.vault.0);
        let registry = registryCall::abi_decode_returns(&self.call(vault, registryCall {}.abi_encode(), at).await?)?;
        if registry.is_zero()
            || !rotationEnabledCall::abi_decode_returns(
                &self.call(registry, rotationEnabledCall {}.abi_encode(), at).await?,
            )?
        {
            return Ok(None);
        }
        let pending = pendingElectionCall::abi_decode_returns(
            &self.call(registry, pendingElectionCall {}.abi_encode(), at).await?,
        )?;
        let in_effect = operatorInEffectCall::abi_decode_returns(
            &self.call(registry, operatorInEffectCall {}.abi_encode(), at).await?,
        )?;
        if (pending.effectiveAt == 0 || pending.adminKey != candidate.0) && in_effect.adminKey != candidate.0 {
            return Ok(None);
        }
        // The incumbent still checkpointing means a normal election, which hands the keys over
        // (#44); the standby takes over only after the liveness timeout, as `promote` on chain
        // already required.
        let liveness =
            parametersCall::abi_decode_returns(&self.call(registry, parametersCall {}.abi_encode(), at).await?)?
                .livenessTimeout;
        let last = lastCheckpointAtCall::abi_decode_returns(
            &self.call(vault, lastCheckpointAtCall {}.abi_encode(), at).await?,
        )?;
        if time < last.saturating_add(liveness) {
            return Ok(None);
        }
        let has =
            hasCheckpointCall::abi_decode_returns(&self.call(vault, hasCheckpointCall {}.abi_encode(), at).await?)?;
        ensure!(has, "the vault holds no checkpoint; a takeover has nothing to resume from");
        let seq = latestSeqCall::abi_decode_returns(&self.call(vault, latestSeqCall {}.abi_encode(), at).await?)?;
        let root = latestRootCall::abi_decode_returns(&self.call(vault, latestRootCall {}.abi_encode(), at).await?)?;
        let signer = checkpointSignerAtCall::abi_decode_returns(
            &self.call(vault, checkpointSignerAtCall { seq }.abi_encode(), at).await?,
        )?;
        Ok(Some(Takeover {
            seq,
            root: format!("0x{}", hex::encode(root.0)),
            signer: engine_core::types::AccountId(signer.0.0),
        }))
    }
}
