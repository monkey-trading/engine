//! `[genesis.carry]`: the log whose final checkpoint a new log's genesis carries MONKEY entitlements and awards
//! from, and whose stakes it mirrors again, at a move to a new vault (`engine_core::carry`, `crate::carry`).

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
    /// - no `[genesis.staking]`, which the stakes mirrored again need initialised first
    /// - custody other than EVM, or EVM custody without `[custody.evm.staking]`: the start reads the source
    ///   vault, the registry and the minter through the custody quorum, and the stakes are mirrored again
    ///   from the staking contract
    /// - no `custody.evm.staking.deployment_block`, from which the carried stakes are mirrored again
    /// - the source vault is this node's own
    pub(super) fn validate_carry(&self) -> anyhow::Result<()> {
        let Some(carry) = &self.genesis.carry else { return Ok(()) };
        ensure!(
            self.genesis.staking.is_some(),
            "[genesis.carry] needs [genesis.staking], which the stakes mirrored again join"
        );
        let staking = self.custody.evm.as_ref().and_then(|evm| evm.staking.as_ref());
        let Some(staking) = staking.filter(|_| self.custody.provider == CustodyProvider::Evm) else {
            bail!("[genesis.carry] needs EVM custody with [custody.evm.staking], whose contracts its start reads");
        };
        ensure!(
            staking.deployment_block.is_some(),
            "[genesis.carry] needs custody.evm.staking.deployment_block, from which the carried stakes are mirrored again"
        );
        let vault: AccountId = self.custody.vault.parse().map_err(|_| anyhow::anyhow!("custody.vault"))?;
        ensure!(carry.vault != vault, "genesis.carry.vault is the source log's vault, not this node's");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use engine_core::staking::StakingConfig;

    use super::*;
    use crate::test_support::ANVIL_CONFIG;

    /// A config with `[genesis.carry]` needs the staking contract's deployment block, from which the carried stakes
    /// are mirrored again, beside `[custody.evm.staking]` and `[genesis.staking]`.
    #[test]
    fn a_carry_needs_the_staking_contracts_deployment_block() {
        let mut cfg = NodeConfig::load(Path::new(ANVIL_CONFIG)).expect("the Anvil config loads");
        cfg.genesis.staking = Some(StakingConfig {
            liquidity_floor: engine_core::Fixed::from_int(1),
            epoch_ms: 86_400_000,
            start_ms: 86_400_000,
            cooldown_ms: 3 * 86_400_000,
        });
        cfg.genesis.carry = Some(CarrySection { vault: AccountId([0xa1; 20]), file: "carry.json".into() });
        let staking = |deployment: &str| {
            let section = format!(
                "contract = '0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a'\ncursor_path = 'staking.json'\n{deployment}\
                 [registry]\ncontract = '0x6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b'"
            );
            Some(toml::from_str(&section).unwrap())
        };
        cfg.custody.evm.as_mut().unwrap().staking = staking("");
        let refused = cfg.validate_carry().unwrap_err();
        assert!(format!("{refused:#}").contains("deployment_block"), "{refused:#}");
        cfg.custody.evm.as_mut().unwrap().staking = staking("deployment_block = 12\n");
        cfg.validate_carry().unwrap();
    }
}
