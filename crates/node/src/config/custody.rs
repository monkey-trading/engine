//! The `[custody.evm]` section, the independent providers whose URLs may carry credentials, and the checks that tie
//! the custody section to its provider.

use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use alloy::primitives::Address;
use alloy::transports::http::reqwest::Url;
use anyhow::bail;
use engine_core::Fixed;
use serde::Deserialize;

use crate::config::{CustodyProvider, NodeConfig, TeeProvider};

/// The block tag whose number marks finality on the custody chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalityTag {
    Latest,
    /// On Arbitrum One, the newest block whose L2 batch is posted to Ethereum.
    Safe,
    Finalized,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvmCustodySection {
    /// HTTPS unless it is on this machine (`127.0.0.1` or `[::1]`). It must serve state at past
    /// blocks back to `start_block` (an archive node), which catch-up after downtime and rescans
    /// read.
    pub providers: Vec<RpcProviderSection>,
    /// The USDC contract the vault must hold; the watcher checks it on every poll.
    pub usdc: Address,
    /// Trading credit follows this tag, less `confirmations`; production requires `latest`.
    /// Earlier protocols use this tag as the sole final-deposit rule.
    pub finality_tag: FinalityTag,
    /// Releases confirmed principal at this tag, without another confirmations delay.
    pub withdrawal_finality_tag: FinalityTag,
    pub confirmations: u64,
    /// The vault's deployment block, where scanning starts when there is no cursor.
    pub start_block: u64,
    /// Most blocks asked for in one `eth_getLogs` request, and most headers in one JSON-RPC
    /// batch; set it within both of the provider's limits. The watcher also keeps one proven
    /// block hash per this many blocks of backlog.
    pub max_block_range: u64,
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
    /// Deadline for each RPC request.
    #[serde(with = "humantime_serde")]
    pub rpc_timeout: Duration,
    /// A deposit stall, reads trailing the final block, an endpoint head that stops
    /// advancing, or a checkpoint deposit left uncredited, that lasts this long raises a
    /// Critical alert. Sized for the credit tag's cadence: on Arbitrum One, above the
    /// longest gap between L1 batch posts.
    #[serde(with = "humantime_serde")]
    pub stall_critical_after: Duration,
    /// The release tag (`withdrawal_finality_tag`, `finalized` in production) may trail a
    /// moving head, and a restarted node's pending checkpoint inclusion may await that tag
    /// at startup, for this long before `custody_finality_stalled` pages or startup refuses
    /// to open the log. Sized for the release tag's cadence, not the batch-posting cadence:
    /// on Arbitrum One the `finalized` tag follows batch posting by one Ethereum finality
    /// period (about 15 minutes), so production sets this to 30m.
    #[serde(with = "humantime_serde")]
    pub finality_stalled_after: Duration,
    /// Where the deposit cursor is kept between restarts.
    pub cursor_path: PathBuf,
}

/// Independently operated RPC service; its name identifies alerts without exposing credentials.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcProviderSection {
    pub name: String,
    pub rpc_url: SecretUrl,
}

/// A URL that may carry credentials. `Debug` hides it; read it with `expose` where it is used.
#[derive(Deserialize)]
#[serde(transparent)]
pub struct SecretUrl(String);

impl fmt::Debug for SecretUrl {
    /// Never prints the URL.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretUrl(<redacted>)")
    }
}

impl SecretUrl {
    /// The URL, for the one place that connects with it.
    /// Error cases:
    /// - the value is not a valid URL
    pub fn expose(&self) -> anyhow::Result<Url> {
        // `url::ParseError` names the problem, never the input, so nothing secret reaches a log.
        Url::parse(&self.0).map_err(|e| anyhow::anyhow!("custody.evm.providers.rpc_url: {e}"))
    }
}

impl NodeConfig {
    /// Checks the custody section against its provider, and that genesis credits house capital
    /// only under mock custody.
    /// Error cases:
    /// - `provider = "evm"` without `[custody.evm]`, or `provider = "mock"` with it
    /// - `custody.evm.providers[].rpc_url` is not a URL, or is plain HTTP to another machine, where the host
    ///   could rewrite every answer
    /// - a range, interval, or timeout in `[custody.evm]` is zero
    /// - `genesis.house_capital` is not zero with EVM custody
    pub(super) fn validate_custody(&self) -> anyhow::Result<()> {
        let evm = match (self.custody.provider, &self.custody.evm) {
            (CustodyProvider::Mock, None) => return Ok(()),
            (CustodyProvider::Mock, Some(_)) => bail!("[custody.evm] is set but custody.provider is \"mock\""),
            (CustodyProvider::Evm, None) => bail!("custody.provider = \"evm\" needs a [custody.evm] section"),
            (CustodyProvider::Evm, Some(evm)) => evm,
        };
        if self.tee.provider != TeeProvider::Mock
            && (evm.finality_tag != FinalityTag::Latest || evm.withdrawal_finality_tag != FinalityTag::Finalized)
        {
            bail!("production custody requires finality_tag = latest and withdrawal_finality_tag = finalized");
        }
        if evm.providers.len() < 3 {
            bail!("custody.evm.providers requires at least three independent RPC providers");
        }
        let (mut names, mut urls) = (BTreeSet::new(), BTreeSet::new());
        for provider in &evm.providers {
            let url = provider.rpc_url.expose()?;
            if provider.name.trim().is_empty() || !names.insert(&provider.name) || !urls.insert(url.clone()) {
                bail!("custody.evm.providers requires non-empty unique names and distinct URLs");
            }
            // Literal addresses only: `localhost` resolves through the host-controlled `/etc/hosts`.
            let on_this_machine = matches!(url.host_str(), Some("127.0.0.1" | "[::1]"));
            if url.scheme() != "https" && !(url.scheme() == "http" && on_this_machine) {
                bail!("custody.evm.providers.rpc_url must use https unless it is on this machine");
            }
        }
        if evm.max_block_range == 0 || evm.poll_interval.is_zero() || evm.rpc_timeout.is_zero() {
            bail!("custody.evm.max_block_range, poll_interval, and rpc_timeout must be non-zero");
        }
        if evm.finality_stalled_after.is_zero() {
            bail!("custody.evm.finality_stalled_after must be non-zero");
        }
        if self.genesis.house_capital != Fixed::ZERO {
            bail!("genesis.house_capital must be zero with EVM custody; deposit to the house instead");
        }
        Ok(())
    }
}
