//! Optional on-chain checkpoint relayer configuration.

use std::time::Duration;

use alloy::primitives::Address;
use anyhow::{Context, ensure};
use serde::Deserialize;

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
}

impl NodeConfig {
    /// Validates checkpoint transport and refuses hardware custody without rollback protection.
    /// Error cases:
    /// - hardware EVM custody lacks checkpoints, or checkpoints use mock custody
    /// - the endpoint is insecure off-machine, a deadline is zero, or the relayer is zero
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
        Ok(())
    }
}
