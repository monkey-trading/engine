//! `[genesis.carry]`: the log whose final checkpoint a new log's genesis carries MONKEY, stakes and awards from,
//! at a move to a new vault (`engine_core::carry`, `crate::carry`).

use std::path::PathBuf;

use anyhow::{bail, ensure};
use engine_core::types::AccountId;
use serde::Deserialize;

use crate::config::{CustodyProvider, NodeConfig};

/// Where a carried genesis comes from. The chain is the custody chain; the source's checkpoint is the one its
/// vault last accepted, read at first boot, so the file itself is not pinned: the engine proves every value
/// it carries against that checkpoint's root.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarrySection {
    /// The source log's vault, whose signer the guardian has retired.
    pub vault: AccountId,
    /// The carry file `engine carry` wrote from the source's published log.
    pub file: PathBuf,
}

impl NodeConfig {
    /// Checks `[genesis.carry]`, when present.
    /// Error cases:
    /// - no `[genesis.staking]`, which the carry's stakes need initialised first
    /// - custody other than EVM, or EVM custody without `[custody.evm.staking]`: the start reads the source
    ///   vault, the registry and the minter through the custody quorum, and the stakes continue the mirror
    /// - the source vault is this node's own
    pub(super) fn validate_carry(&self) -> anyhow::Result<()> {
        let Some(carry) = &self.genesis.carry else { return Ok(()) };
        ensure!(
            self.genesis.staking.is_some(),
            "[genesis.carry] needs [genesis.staking], which the carry's stakes join"
        );
        let staked = self.custody.evm.as_ref().is_some_and(|evm| evm.staking.is_some());
        if self.custody.provider != CustodyProvider::Evm || !staked {
            bail!("[genesis.carry] needs EVM custody with [custody.evm.staking], whose contracts its start reads");
        }
        let vault: AccountId = self.custody.vault.parse().map_err(|_| anyhow::anyhow!("custody.vault"))?;
        ensure!(carry.vault != vault, "genesis.carry.vault is the source log's vault, not this node's");
        Ok(())
    }
}
