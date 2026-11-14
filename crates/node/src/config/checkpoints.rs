//! Optional on-chain checkpoint relayer configuration.

use std::time::Duration;

use alloy::primitives::utils::parse_ether;
use alloy::primitives::{Address, U256};
use anyhow::{Context, ensure};
use serde::{Deserialize, Deserializer};

use crate::config::{CustodyProvider, NodeConfig, SecretUrl, TeeProvider};

/// The RPC endpoint signs and funds relayer transactions; it has no authority over vault funds.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointSection {
    pub rpc_url: SecretUrl,
    pub relayer: Address,
    #[serde(with = "humantime_serde")]
    pub retry_interval: Duration,
    #[serde(with = "humantime_serde")]
    pub rpc_timeout: Duration,
    /// Present, the node reads the relayer's balance each retry and warns when it runs low.
    #[serde(default)]
    pub relayer_gas: Option<RelayerGasSection>,
}

/// When the relayer's gas balance is low enough to warn about.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayerGasSection {
    /// The balance below which `checkpoint_relayer_low` fires, in ether, e.g. `"0.01"`.
    #[serde(deserialize_with = "ether")]
    pub min_balance: U256,
}

/// Reads a decimal ether amount as wei.
fn ether<'de, D: Deserializer<'de>>(deserializer: D) -> Result<U256, D::Error> {
    let amount = String::deserialize(deserializer)?;
    parse_ether(&amount).map_err(serde::de::Error::custom)
}

/// The hourly default posting interval (see #541).
fn default_log_heads_interval() -> Duration {
    Duration::from_secs(3600)
}

/// On-chain log-head commitments: the node posts its newest released `(seq, link)` at most every
/// `interval` and refuses to start on a log that lacks the posted head (#541).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogHeadsSection {
    /// The LogHeads companion contract on the custody chain.
    pub contract: Address,
    /// The dedicated relayer account the checkpoint endpoint funds. It must differ from the
    /// checkpoint relayer, whose nonce tracking assumes it alone uses its relayer.
    pub relayer: Address,
    /// How often the poster reposts the newest released head; a post lands only when the head moved.
    #[serde(with = "humantime_serde", default = "default_log_heads_interval")]
    pub interval: Duration,
}

impl NodeConfig {
    /// Validates checkpoint transport and refuses hardware custody without rollback protection.
    /// Error cases:
    /// - hardware EVM custody lacks checkpoints, or checkpoints use mock custody
    /// - the endpoint is insecure off-machine, a deadline is zero, the relayer is zero, or the gas
    ///   threshold is zero
    pub(super) fn validate_checkpoints(&self) -> anyhow::Result<()> {
        let Some(cfg) = &self.checkpoints else {
            ensure!(
                self.custody.provider != CustodyProvider::Evm || self.tee.provider == TeeProvider::Mock,
                "hardware EVM custody requires [checkpoints]"
            );
            return Ok(());
        };
        ensure!(self.custody.provider == CustodyProvider::Evm, "[checkpoints] requires EVM custody");
        let url = cfg.rpc_url.attested().context("checkpoints.rpc_url")?;
        ensure!(
            url.scheme() == "https" || matches!(url.host_str(), Some("127.0.0.1" | "[::1]")),
            "checkpoints.rpc_url must use HTTPS off-machine"
        );
        ensure!(!cfg.retry_interval.is_zero() && !cfg.rpc_timeout.is_zero(), "checkpoint deadlines must be non-zero");
        ensure!(!cfg.relayer.is_zero(), "checkpoint relayer must be non-zero");
        ensure!(
            cfg.relayer_gas.as_ref().is_none_or(|gas| !gas.min_balance.is_zero()),
            "checkpoints.relayer_gas.min_balance must be non-zero"
        );
        Ok(())
    }

    /// Validates log-head transport and its checkpoint-endpoint dependency.
    /// Error cases:
    /// - `operator.rotation_enabled` is set without `[log_heads]`
    /// - `[log_heads]` is set without `[checkpoints]`, names the zero contract or relayer, reuses
    ///   the checkpoint relayer, or sets a zero interval
    pub(super) fn validate_log_heads(&self) -> anyhow::Result<()> {
        let Some(cfg) = &self.log_heads else {
            // A successor checks the tail's head against the latest posted one at each handover.
            ensure!(
                !self.operator.rotation_enabled,
                "operator.rotation_enabled = true requires [log_heads]: a rotating node's handovers check the log \
                 against the head posted on chain"
            );
            return Ok(());
        };
        let checkpoints = self.checkpoints.as_ref().context("[log_heads] requires [checkpoints]")?;
        ensure!(!cfg.contract.is_zero(), "log_heads.contract must be non-zero");
        ensure!(!cfg.relayer.is_zero(), "log_heads.relayer must be non-zero");
        ensure!(cfg.relayer != checkpoints.relayer, "log_heads.relayer must differ from the checkpoint relayer");
        ensure!(!cfg.interval.is_zero(), "log_heads.interval must be non-zero");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::test_support::ANVIL_CONFIG;

    use super::*;

    #[test]
    fn relayer_gas_reads_ether_and_a_zero_minimum_is_refused() {
        let section: CheckpointSection = toml::from_str(
            "rpc_url = 'http://127.0.0.1:8545'\n\
             relayer = '0x1111111111111111111111111111111111111111'\n\
             retry_interval = '2s'\n\
             rpc_timeout = '10s'\n\
             [relayer_gas]\n\
             min_balance = '0.01'",
        )
        .unwrap();
        assert_eq!(section.relayer_gas.unwrap().min_balance, parse_ether("0.01").unwrap());

        let mut cfg = NodeConfig::load(Path::new(ANVIL_CONFIG)).unwrap();
        cfg.checkpoints = Some(
            toml::from_str(
                "rpc_url = 'http://127.0.0.1:8545'\n\
                 relayer = '0x1111111111111111111111111111111111111111'\n\
                 retry_interval = '2s'\n\
                 rpc_timeout = '10s'\n\
                 [relayer_gas]\n\
                 min_balance = '0'",
            )
            .unwrap(),
        );
        let err = cfg.validate_checkpoints().unwrap_err();
        assert!(err.to_string().contains("min_balance must be non-zero"), "{err:#}");
        cfg.checkpoints.as_mut().unwrap().relayer_gas.as_mut().unwrap().min_balance = parse_ether("0.01").unwrap();
        cfg.validate_checkpoints().unwrap();
    }
}
