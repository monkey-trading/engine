//! Optional on-chain checkpoint relayer configuration.

use std::time::Duration;

use alloy::primitives::utils::parse_ether;
use alloy::primitives::{Address, U256};
use anyhow::{Context, ensure};
use serde::{Deserialize, Deserializer};

use crate::config::{CustodyProvider, NodeConfig, SecretUrl, TeeProvider};

/// The RPC endpoint signs and funds relayer transactions; it has no authority over vault funds.
/// Required with EVM custody under real attestation, refused with mock custody.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointSection {
    /// The endpoint must answer `eth_sendTransaction` for the relayer, `eth_getTransactionByHash`,
    /// `eth_getTransactionCount` at `latest`, `pending` and past blocks, and whole blocks with their
    /// transactions; without that history a reserved nonce stays reserved.
    pub rpc_url: SecretUrl,
    /// A funded account no other sender uses: posting pauses on pending transactions or a nonce
    /// consumer that are not this log's (`checkpoints::rpc::submission`).
    pub relayer: Address,
    /// A failed post retries after this, raising `checkpoint_post_failed`.
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

/// The poster's default read-back cadence outside a turn: a post lands at most once an hour there, so a
/// read every 30 s confirms each well before the next, finds another copy's head within 30 s, and
/// matches the cadence the liveness rule's heartbeat holds it to with EVM custody.
fn default_log_heads_read_interval() -> Duration {
    Duration::from_secs(30)
}

/// The liveness reads' default cadence while no heartbeat interval is known.
pub(crate) fn default_liveness_read_interval() -> Duration {
    Duration::from_secs(15)
}

/// The fallback fixer's default cadence, the longest a production build allows.
fn default_fallback_interval() -> Duration {
    PRODUCTION_FALLBACK_INTERVAL
}

/// The longest `[liveness] fallback_interval` a production build allows. Once anyone fixes a turn's
/// fallback block, its hash can be recorded for 256 blocks, 64 s on Arbitrum One: a read every 30 s
/// sees the fix and records the hash with time to spare, even alone.
const PRODUCTION_FALLBACK_INTERVAL: Duration = Duration::from_secs(30);

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
    /// How often the poster reads the posted head back, and at least every `interval`. Wherever the
    /// liveness rule runs, as with EVM custody, it reads at least every 30 s, while a turn names the
    /// operator every sixth of its heartbeat interval, and while none does every second from each turn's
    /// boundary, by trusted time, until 30 s past it or the first head of a turn it found.
    #[serde(with = "humantime_serde", default = "default_log_heads_read_interval")]
    pub read_interval: Duration,
}

/// A rotation-set member's liveness caller (#595): the account the checkpoint endpoint signs its
/// witnesses, promotions and relayed heads for, funded apart from every relayer.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LivenessSection {
    /// The caller's account. It must differ from the checkpoint, log-head and seed relayers.
    pub caller: Address,
    /// The balance below which `liveness_caller_unfunded` fires, in ether, e.g. `"0.01"`.
    #[serde(deserialize_with = "ether")]
    pub min_balance: U256,
    /// How often the caller and the operator's rule read the chain while they know no heartbeat interval,
    /// as before a schedule is named; with one, each reads four times an interval whatever this says.
    #[serde(with = "humantime_serde", default = "default_liveness_read_interval")]
    pub read_interval: Duration,
    /// How often the fallback fixer reads the seed receiver, and every 5 s once a fix is under way, until
    /// the fixed block's hash is recorded.
    #[serde(with = "humantime_serde", default = "default_fallback_interval")]
    pub fallback_interval: Duration,
}

impl LivenessSection {
    /// Refuses a fallback fixer slower than the production value when `production` is set.
    /// Error cases:
    /// - `production` is set and `fallback_interval` is above `PRODUCTION_FALLBACK_INTERVAL`
    pub(super) fn refuse_slow_fallback_fixer(&self, production: bool) -> anyhow::Result<()> {
        ensure!(
            !production || self.fallback_interval <= PRODUCTION_FALLBACK_INTERVAL,
            "a production build allows liveness.fallback_interval at most {PRODUCTION_FALLBACK_INTERVAL:?}, and the \
             config sets {:?}",
            self.fallback_interval
        );
        Ok(())
    }
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
    ///   the checkpoint relayer, or sets a zero interval or read interval
    /// - `[liveness]` names the zero caller or one a relayer uses, a zero minimum balance, or a zero
    ///   read or fallback interval
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
        ensure!(!cfg.read_interval.is_zero(), "log_heads.read_interval must be non-zero");
        let Some(liveness) = &self.liveness else { return Ok(()) };
        ensure!(!liveness.caller.is_zero(), "liveness.caller must be non-zero");
        let seed_relayer = self.seed_relay.as_ref().map(|seed| seed.relayer);
        ensure!(
            ![Some(checkpoints.relayer), Some(cfg.relayer), seed_relayer].contains(&Some(liveness.caller)),
            "liveness.caller must differ from the checkpoint, log-head and seed relayers"
        );
        ensure!(!liveness.min_balance.is_zero(), "liveness.min_balance must be non-zero");
        ensure!(
            !liveness.read_interval.is_zero() && !liveness.fallback_interval.is_zero(),
            "liveness.read_interval and liveness.fallback_interval must be non-zero"
        );
        Ok(())
    }

    /// Refuses `[liveness]` without `[log_heads]`.
    /// Error cases:
    /// - `[liveness]` is set without `[log_heads]`, whose contract and heads it reads
    pub(super) fn validate_liveness(&self) -> anyhow::Result<()> {
        ensure!(
            self.liveness.is_none() || self.log_heads.is_some(),
            "[liveness] requires [log_heads]: the caller relays and reads its heads"
        );
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

    #[test]
    fn read_intervals_default_to_their_cadences_and_a_production_build_bounds_the_fixer() {
        let liveness: LivenessSection =
            toml::from_str("caller = '0x1111111111111111111111111111111111111111'\nmin_balance = '0.01'").unwrap();
        assert_eq!(
            (liveness.read_interval, liveness.fallback_interval),
            (Duration::from_secs(15), Duration::from_secs(30))
        );
        let heads: LogHeadsSection = toml::from_str(
            "contract = '0x1111111111111111111111111111111111111111'\n\
             relayer = '0x2222222222222222222222222222222222222222'",
        )
        .unwrap();
        assert_eq!((heads.interval, heads.read_interval), (Duration::from_secs(3600), Duration::from_secs(30)));

        let mut liveness = LivenessSection { fallback_interval: PRODUCTION_FALLBACK_INTERVAL, ..liveness };
        liveness.refuse_slow_fallback_fixer(true).unwrap();
        liveness.fallback_interval += Duration::from_secs(1);
        let err = liveness.refuse_slow_fallback_fixer(true).unwrap_err();
        assert!(err.to_string().contains("liveness.fallback_interval"), "{err:#}");
        liveness.refuse_slow_fallback_fixer(false).unwrap();
    }
}
