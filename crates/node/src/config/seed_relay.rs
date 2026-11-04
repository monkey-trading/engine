//! The optional `[seed_relay]` section: the seed relayer, which proves each daily operator turn's
//! RANDAO mix on Ethereum and relays it to the seed receiver on Arbitrum.

use std::time::Duration;

use alloy::primitives::utils::parse_units;
use alloy::primitives::{Address, U256};
use anyhow::{Context, ensure};
use serde::{Deserialize, Deserializer};

use crate::config::custody::check_providers;
use crate::config::{CustodyProvider, NodeConfig, RpcProviderSection, SecretUrl};

/// Where the relayer reads Ethereum and the beacon chain, and what it sends with.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedRelaySection {
    /// Independently operated Ethereum execution providers, at least three, a strict majority of
    /// which must answer every read the same.
    pub providers: Vec<RpcProviderSection>,
    /// The chain the providers must serve: 1 on mainnet.
    pub chain_id: u64,
    /// Signs and funds the relay transactions as `relayer`. It holds no key of the node's.
    pub rpc_url: SecretUrl,
    /// The relayer's address, which must not be the checkpoint relayer's.
    pub relayer: Address,
    /// A beacon node's API, which serves the finalized block's header and its state.
    pub beacon_api: SecretUrl,
    /// The `SeedRelay` contract on Ethereum.
    pub relay: Address,
    /// The `SeedReceiver` contract on Arbitrum, read through the custody providers.
    pub receiver: Address,
    /// How often the relayer reads both chains and relays what the receiver lacks.
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
    /// Deadline for each RPC and beacon API request but the state's.
    #[serde(with = "humantime_serde")]
    pub rpc_timeout: Duration,
    /// Deadline for downloading one beacon state, about 340 MB on mainnet.
    #[serde(with = "humantime_serde")]
    pub state_timeout: Duration,
    /// How long a seed relayed from this node may take to reach Arbitrum before it is relayed again.
    #[serde(with = "humantime_serde")]
    pub resend_after: Duration,
    /// The most a relay's ticket pays per unit of Arbitrum gas, in gwei, e.g. `"0.1"`; the ticket's
    /// unspent fees are refunded to `relayer` on Arbitrum.
    #[serde(deserialize_with = "gwei")]
    pub l2_max_fee_per_gas: U256,
    /// The relayer's balance on Ethereum below which `seed_relayer_unfunded` fires, in ether.
    #[serde(deserialize_with = "ether")]
    pub min_balance: U256,
}

/// Reads a decimal gwei amount as wei.
fn gwei<'de, D: Deserializer<'de>>(deserializer: D) -> Result<U256, D::Error> {
    units(deserializer, "gwei")
}

/// Reads a decimal ether amount as wei.
fn ether<'de, D: Deserializer<'de>>(deserializer: D) -> Result<U256, D::Error> {
    units(deserializer, "ether")
}

/// Reads a non-negative decimal amount of `unit` as wei; the parser takes a negative amount's
/// absolute value, so it is refused first.
fn units<'de, D: Deserializer<'de>>(deserializer: D, unit: &str) -> Result<U256, D::Error> {
    let amount = String::deserialize(deserializer)?;
    let parsed = parse_units(&amount, unit).map_err(serde::de::Error::custom)?;
    if parsed.is_negative() {
        return Err(serde::de::Error::custom(format!("{amount} {unit} is negative")));
    }
    Ok(parsed.into())
}

impl SeedRelaySection {
    /// Every URL the relayer reads, with its config field.
    pub(crate) fn urls(&self) -> Vec<(&SecretUrl, String)> {
        let providers = self
            .providers
            .iter()
            .map(|provider| (&provider.rpc_url, format!("seed_relay.providers[{}].rpc_url", provider.name)));
        providers
            .chain([
                (&self.rpc_url, "seed_relay.rpc_url".to_owned()),
                (&self.beacon_api, "seed_relay.beacon_api".to_owned()),
            ])
            .collect()
    }

    /// Whether every URL's delivered key, if it names one, is here.
    pub(crate) fn delivered(&self) -> bool {
        self.urls().iter().all(|(url, _)| url.expose().is_ok())
    }
}

impl NodeConfig {
    /// Checks the seed relayer against the custody section it reads Arbitrum through.
    /// Error cases:
    /// - `[seed_relay]` without EVM custody, whose providers read the receiver
    /// - its providers fail `check_providers`, or `rpc_url` or `beacon_api` is plain HTTP to another
    ///   machine
    /// - the relayer is zero or the checkpoint relayer, the relay or receiver is zero, a deadline or
    ///   interval is zero, or a fee or balance threshold is zero
    pub(super) fn validate_seed_relay(&self) -> anyhow::Result<()> {
        let Some(cfg) = &self.seed_relay else { return Ok(()) };
        ensure!(self.custody.provider == CustodyProvider::Evm, "[seed_relay] reads its receiver through EVM custody");
        check_providers(&cfg.providers, "seed_relay.providers")?;
        for (field, url) in [("seed_relay.rpc_url", &cfg.rpc_url), ("seed_relay.beacon_api", &cfg.beacon_api)] {
            let url = url.attested().context(field)?;
            let on_this_machine = matches!(url.host_str(), Some("127.0.0.1" | "[::1]"));
            ensure!(
                url.scheme() == "https" || (url.scheme() == "http" && on_this_machine),
                "{field} must use https unless it is on this machine"
            );
        }
        ensure!(!cfg.relayer.is_zero(), "seed_relay.relayer must be non-zero");
        ensure!(
            self.checkpoints.as_ref().is_none_or(|checkpoints| checkpoints.relayer != cfg.relayer),
            "seed_relay.relayer must be a key apart from checkpoints.relayer"
        );
        ensure!(!cfg.relay.is_zero() && !cfg.receiver.is_zero(), "seed_relay.relay and receiver must be non-zero");
        ensure!(
            [cfg.poll_interval, cfg.rpc_timeout, cfg.state_timeout, cfg.resend_after].iter().all(|d| !d.is_zero()),
            "seed_relay deadlines and intervals must be non-zero"
        );
        ensure!(
            !cfg.l2_max_fee_per_gas.is_zero() && !cfg.min_balance.is_zero(),
            "seed_relay.l2_max_fee_per_gas and min_balance must be non-zero"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
