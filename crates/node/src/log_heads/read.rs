//! The latest posted log head, read for a member that follows the operator in effect: its replica
//! keeps the head as it passes it, and its tail handover requires the copy to hold it.

use std::sync::Arc;

use anyhow::Context;

use super::rpc::HeadsRpc;
use super::{Head, HeadsChain};
use crate::config::NodeConfig;
use crate::custody::heads_reader;
use crate::log::CheckpointDomain;

/// Reads the vault's current signer epoch's posted head through the custody RPC quorum.
pub(crate) struct PostedHeads {
    chain: HeadsRpc,
}

impl PostedHeads {
    /// The reader `[log_heads]` configures; none without the section.
    /// Error cases:
    /// - the section is set without `[checkpoints]`, or a client cannot be built
    pub(crate) fn new(cfg: &NodeConfig) -> anyhow::Result<Option<Self>> {
        let Some(log_heads) = &cfg.log_heads else { return Ok(None) };
        let checkpoints = cfg.checkpoints.as_ref().context("[log_heads] requires [checkpoints]")?;
        let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
        let quorum = Arc::new(heads_reader(&cfg.custody)?);
        Ok(Some(Self { chain: HeadsRpc::new(checkpoints, log_heads, domain, &cfg.node.log_dir, quorum)? }))
    }

    /// The newest head the vault's current signer epoch holds, if any.
    /// Error cases:
    /// - the quorum read fails
    pub(crate) async fn latest(&self) -> anyhow::Result<Option<Head>> {
        Ok(self.chain.posted().await?.1)
    }
}
