//! The operator list `GET /v1/operators` serves: the registry read through its `RegistryLens` over
//! the custody RPC quorum, at one block the providers' majority declares final, every
//! `api.operators_interval`. The host supplies the chain data, so nothing is taken from one
//! provider: the lens's return data must be identical across a strict majority, executed in the
//! canonical block they agreed on, and the answer names that block, its hash and its time. A read
//! that fails leaves the last list in place for as long as `custody.evm.finality_stalled_after`,
//! then the route answers 503 until a read succeeds. The reader has its own quorum state, apart
//! from the deposit watcher's.

mod lens;
#[cfg(test)]
pub(crate) mod tests;

use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;
use anyhow::{Context, bail, ensure};
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, info, warn};

pub use lens::OperatorList;
use lens::{BlockStamp, Reads, candidatesCall, overviewCall, registryCall};

use crate::config::{ApiSection, CustodySection};
use crate::custody::evm::{BlockRef, VaultReader};
use crate::custody::quorum::Quorum;
use crate::custody::rpc::VaultRpc;

/// Candidates per `candidates` call: at 100 listed, four calls, each well inside a public endpoint's
/// gas cap (one call for all 100 costs about 4M gas).
const PAGE: usize = 25;

/// A list and when it was read.
struct Read {
    list: Arc<OperatorList>,
    at: Instant,
}

/// What the API serves `/v1/operators` from.
#[derive(Clone)]
pub struct OperatorsFeed {
    reads: watch::Receiver<Option<Arc<Read>>>,
    stale_after: Duration,
}

/// Why there is no list to serve.
#[derive(Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// No read has succeeded yet.
    NotRead,
    /// The last read is older than the stale bound, and the reads since have failed.
    Stale,
}

impl OperatorsFeed {
    /// The list the last successful read made, while it is fresh.
    /// Error cases:
    /// - no read has succeeded yet, or the last one is older than `custody.evm.finality_stalled_after`
    pub fn current(&self) -> Result<Arc<OperatorList>, Unavailable> {
        let read = self.reads.borrow().clone().ok_or(Unavailable::NotRead)?;
        if read.at.elapsed() > self.stale_after {
            return Err(Unavailable::Stale);
        }
        Ok(read.list.clone())
    }
}

/// Reads the operator list through the quorum `R` forms.
pub struct OperatorsReader<R> {
    quorum: Quorum<R>,
    chain_id: u64,
    usdc: Address,
    registry: Address,
    lens: Address,
    every: Duration,
    reads: watch::Sender<Option<Arc<Read>>>,
}

impl OperatorsReader<VaultRpc> {
    /// The reader and its feed for the configured lens; `None` without a lens, or without EVM custody.
    /// Builds clients without accessing the network.
    /// Error cases:
    /// - `custody.vault` or a provider's client does not parse or build
    pub fn connect(custody: &CustodySection, api: &ApiSection) -> anyhow::Result<Option<(Self, OperatorsFeed)>> {
        let Some(evm) = &custody.evm else { return Ok(None) };
        let Some(staking) = &evm.staking else { return Ok(None) };
        let Some(lens) = staking.registry.lens else { return Ok(None) };
        // A lost read pauses nothing but this list, so its quorum alerts at Warning, apart from the
        // deposit watcher's.
        let quorum = Quorum::connect(evm, custody.vault.parse().context("custody.vault")?)?.named("operators", false);
        let chain = (custody.chain_id, evm.usdc);
        let (registry, every) = (staking.registry.contract, api.operators_interval);
        Ok(Some(Self::new(quorum, chain, (registry, lens), every, evm.finality_stalled_after)))
    }
}

impl<R: VaultReader + Sync> OperatorsReader<R> {
    /// A reader over `quorum` for the chain and token `chain`, the registry and its lens, reading every
    /// `every`, and its feed, which serves a list for `stale_after` after it was read.
    fn new(
        quorum: Quorum<R>,
        chain: (u64, Address),
        (registry, lens): (Address, Address),
        every: Duration,
        stale_after: Duration,
    ) -> (Self, OperatorsFeed) {
        let (reads, rx) = watch::channel(None);
        let feed = OperatorsFeed { reads: rx, stale_after };
        (Self { quorum, chain_id: chain.0, usdc: chain.1, registry, lens, every, reads }, feed)
    }

    /// Reads the list every interval, forever. A failed read is a Warning and leaves the list in place
    /// for the feed's stale bound.
    /// Error cases:
    /// - none: the loop ends only when the node does
    pub async fn run(self) -> anyhow::Result<()> {
        let mut ticks = interval(self.every);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            match self.read().await {
                Ok(list) => {
                    debug!(
                        block = list.block.number,
                        candidates = list.candidate_count(),
                        "operator list read at a final block"
                    );
                    info!(alert_resolved = "operators_read_failed", "the operator list reads again");
                    self.reads.send_replace(Some(Arc::new(Read { list: Arc::new(list), at: Instant::now() })));
                }
                Err(error) => warn!(
                    alert = "operators_read_failed",
                    "the operator list cannot be read at a final block; the last one stays up until it is stale: {error:#}"
                ),
            }
        }
    }

    /// One read: the quorum's chain and token, the block a majority declares final, the lens's registry,
    /// overview and every page, all at that block.
    /// Error cases:
    /// - the providers lack a quorum for any read, serve another chain or token, or name no final block
    /// - the lens answers inconsistently (see `OperatorList::assemble`)
    pub(super) async fn read(&self) -> anyhow::Result<OperatorList> {
        let identity = self.quorum.identity().await?;
        ensure!(
            identity.chain_id == self.chain_id && identity.token == Some(self.usdc),
            "the providers serve chain {} and token {:?}, not the configured chain {} and token {}",
            identity.chain_id,
            identity.token,
            self.chain_id,
            self.usdc
        );
        let at = self.quorum.withdrawal_finality().await?.tagged.context("custody providers name no final block")?;
        let header = self.quorum.headers(at.number, at.number).await?;
        let [header] = header.as_slice() else {
            bail!("the providers returned {} headers for one block", header.len())
        };
        ensure!(
            header.number == at.number && header.hash == at.hash,
            "the final block's header differs from the block the providers declared"
        );
        let time_ms = i64::try_from(header.timestamp)
            .ok()
            .and_then(|seconds| seconds.checked_mul(1000))
            .context("the final block's time is out of range")?;
        let block = BlockStamp { number: at.number, hash: at.hash, time_ms };

        let registry = registryCall::abi_decode_returns_validate(
            &self.call("lens_registry", at, registryCall {}.abi_encode()).await?,
        )
        .context("decoding the lens's registry")?;
        let overview = overviewCall::abi_decode_returns_validate(
            &self.call("lens_overview", at, overviewCall {}.abi_encode()).await?,
        )
        .context("decoding the lens's overview")?;
        let count = usize::try_from(overview.candidateCount)
            .ok()
            .filter(|count| *count <= lens::CANDIDATE_LIMIT)
            .with_context(|| format!("the lens lists {} candidates", overview.candidateCount))?;
        let mut rows = Vec::with_capacity(count);
        while rows.len() < count {
            let start = U256::from(rows.len());
            let limit = U256::from(PAGE);
            let page = candidatesCall::abi_decode_returns_validate(
                &self.call("lens_candidates", at, candidatesCall { start, limit }.abi_encode()).await?,
            )
            .context("decoding a page of the lens's candidates")?;
            ensure!(
                !page.is_empty() && page.len() <= PAGE.min(count - rows.len()),
                "the lens returned {} rows for a page of at most {}",
                page.len(),
                PAGE.min(count - rows.len())
            );
            rows.extend(page);
        }
        OperatorList::assemble(Reads { block, registry, lens: self.lens, overview, rows }, self.registry)
    }

    /// The lens's return data for `input` at `at`, identical across a majority.
    /// Error cases:
    /// - the providers lack a quorum for the call
    async fn call(&self, operation: &'static str, at: BlockRef, input: Vec<u8>) -> anyhow::Result<Bytes> {
        self.quorum.call_as(operation, at, self.lens, input.into()).await
    }
}
