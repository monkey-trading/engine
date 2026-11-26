//! The schedule's governance record at a start, before the node records the configured vote and fault proof
//! contracts in its log: read through the custody quorum at the release tag's final block, as the custody
//! watcher reads it, and only while the registry names the configured schedule there.

use std::future::Future;
use std::sync::Arc;

use alloy::primitives::Address;
use anyhow::{Context, ensure};
use futures_util::future::BoxFuture;

use crate::config::CustodySection;
use crate::custody::evm::{GovernanceRecord, VaultReader};
use crate::custody::{ContractQuorum, Quorum};

/// A reader of the configured schedule's governance record, with the vote and fault proof contracts the config
/// names.
pub struct RecordReader {
    read: Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<GovernanceRecord>> + Send + Sync>,
    configured: (Option<Address>, Option<Address>),
}

impl RecordReader {
    /// The reader over its own custody quorum; `None` without EVM custody, a schedule, or a vote or fault proof
    /// contract configured. Builds clients without accessing the network.
    /// Error cases:
    /// - fewer than three providers, or `custody.vault` or a provider's client does not parse or build
    pub fn connect(custody: &CustodySection) -> anyhow::Result<Option<Self>> {
        let Some((evm, staking)) = custody.evm.as_ref().and_then(|evm| Some((evm, evm.staking.as_ref()?))) else {
            return Ok(None);
        };
        let registry = &staking.registry;
        let configured = (
            registry.vote.as_ref().map(|vote| vote.contract),
            registry.fault_proof.as_ref().map(|fault_proof| fault_proof.contract),
        );
        let Some(schedule) = registry.schedule.filter(|_| configured != (None, None)) else { return Ok(None) };
        let quorum = Arc::new(ContractQuorum::connect(custody, "governance_record")?);
        let chain = (custody.chain_id, evm.usdc);
        Ok(Some(Self::new(configured, move || {
            let quorum = quorum.clone();
            async move { read(quorum.quorum(), schedule, chain).await }
        })))
    }

    /// A reader that calls `read` for each read, for the contracts `configured`.
    fn new<F>(configured: (Option<Address>, Option<Address>), read: impl Fn() -> F + Send + Sync + 'static) -> Self
    where
        F: Future<Output = anyhow::Result<GovernanceRecord>> + Send + 'static,
    {
        Self { read: Arc::new(move || Box::pin(read())), configured }
    }

    /// A reader for the contracts `configured` that answers `record`, or fails when `None`.
    #[cfg(test)]
    pub(crate) fn fixed(configured: (Option<Address>, Option<Address>), record: Option<GovernanceRecord>) -> Self {
        Self::new(configured, move || async move { record.context("the providers name no final block") })
    }

    /// What the record names at the release tag's final block.
    /// Error cases:
    /// - see `read`
    pub async fn read(&self) -> anyhow::Result<GovernanceRecord> {
        (self.read)().await
    }

    /// The vote and fault proof contracts the config names.
    pub fn configured(&self) -> (Option<Address>, Option<Address>) {
        self.configured
    }
}

/// What the governance record of `schedule` names at the release tag's final block, read by a strict majority of
/// providers that serve `chain`, the configured chain id and the vault's token; `Unset` while the registry has no
/// code there or names another schedule, or none, since nothing then vouches for the configured one's record.
/// Error cases:
/// - the providers lack an identity, finality or read quorum, agree on another chain or token than the
///   configured deployment's, or name no final block
pub(super) async fn read<R: VaultReader + Sync>(
    quorum: &Quorum<R>,
    schedule: Address,
    (chain_id, token): (u64, Address),
) -> anyhow::Result<GovernanceRecord> {
    let identity = quorum.identity().await?;
    ensure!(
        (identity.chain_id, identity.token) == (chain_id, Some(token)),
        "the providers serve chain {} and token {:?}, not the configured chain {chain_id} and token {token}",
        identity.chain_id,
        identity.token
    );
    let at = quorum.withdrawal_finality().await?.tagged.context("custody providers name no final block")?;
    if quorum.registry_staking(at).await?.is_none() || quorum.registry_schedule(at).await? != schedule {
        return Ok(GovernanceRecord::Unset);
    }
    quorum.governance_record(at).await
}
