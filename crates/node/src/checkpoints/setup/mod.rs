//! Checkpoint setup before the node opens its log for writing.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use engine_core::types::AccountId;
use tokio::sync::mpsc;

use crate::checkpoints::{
    ApprovalRegistry, CheckpointChain, CheckpointRpc, Observation, Poster, RegistryOperator, RelayerGas, View, Work,
};
use crate::config::{EvmCustodySection, FinalityTag, NodeConfig};
use crate::custody::BalanceQuorum;
use crate::log::CheckpointDomain;
use crate::metrics::Metrics;
use crate::tee::EnclaveKey;

/// The vault names another signer than this enclave's key, at its latest or finalized block.
#[derive(Debug)]
pub(crate) struct Unregistered {
    pub vault_signer: AccountId,
    /// The finalized block names this key and the latest another: a change of signer moves past it.
    pub superseded: bool,
}

pub(crate) struct Setup {
    pub poster: Poster<CheckpointRpc>,
    pub tx: mpsc::Sender<Work>,
    pub rx: mpsc::Receiver<Work>,
    pub approvals: ApprovalRegistry,
    pub observation: Observation,
    pub parent: crate::log::CheckpointParent,
    pub posted_seq: Option<u64>,
    /// The registry's operator in effect at `observation`, while rotation is off.
    pub registry_operator: Option<RegistryOperator>,
}

impl std::fmt::Display for Unregistered {
    /// Names the vault's signer.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the vault names {} as its signer, not this enclave", self.vault_signer)
    }
}

impl std::error::Error for Unregistered {}

/// Checkpoint release follows custody withdrawal finality, independently of early credit depth.
fn release_policy(evm: &EvmCustodySection) -> (FinalityTag, u64) {
    (evm.withdrawal_finality_tag, 0)
}

/// A restart must not sign from a finalized parent while its own latest inclusion is pending.
/// Await that boundary, without opening persisted state, for the release tag's own budget: the
/// tag may trail a moving head past the batch-posting budget, but an orphaned inclusion may
/// disappear.
async fn await_finality_boundary<C: CheckpointChain>(
    chain: &C,
    signer: AccountId,
    retry: Duration,
    evm: &EvmCustodySection,
) -> anyhow::Result<Observation> {
    tokio::time::timeout(evm.finality_stalled_after, async {
        loop {
            let (latest, finalized) = views(chain, signer, false).await?;
            if latest.state.checkpoint == finalized.state.checkpoint && same_epoch(&latest, &finalized) {
                break anyhow::Ok(finalized);
            }
            tokio::time::sleep(retry).await;
        }
    })
    .await
    .context("checkpoint startup awaits custody finality beyond its finality budget")?
}

/// A successor's view of the vault: its finalized checkpoint, with no wait for the latest one to
/// finalize. An incumbent nearly always has one in flight, and every checkpoint it posted past the
/// finalized one is in the successor's copy, which recovery from the replica resumes as outstanding
/// work, so the successor signs past it, never from the finalized parent beside it.
/// Error cases:
/// - see `views`, a change of signer not yet finalized (`EpochUnfinal`) included
async fn at_finality<C: CheckpointChain>(chain: &C, signer: AccountId) -> anyhow::Result<Observation> {
    views(chain, signer, true).await.map(|(_, finalized)| finalized)
}

/// The vault's latest signer epoch is not the finalized one yet, as just after a promotion (#595): a
/// successor's start, which needs the finalized anchor, waits for it, as for a turn drawn within a
/// finality period of its boundary.
#[derive(Debug)]
pub(crate) struct EpochUnfinal;

impl std::fmt::Display for EpochUnfinal {
    /// What holds the start.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the vault's signer epoch is not yet finalized")
    }
}

impl std::error::Error for EpochUnfinal {}

/// The vault's latest view, validated, naming `signer`, unretired.
/// Error cases:
/// - the read or its validation fails, the guardian retired the signer, or the view names another
///   signer (`Unregistered`)
async fn at_latest<C: CheckpointChain>(chain: &C, signer: AccountId) -> anyhow::Result<Observation> {
    let latest = chain.observe(View::Latest, &[]).await?;
    latest.validate(&[])?;
    anyhow::ensure!(!latest.state.signer_retired(), "the vault's guardian retired its signer");
    if latest.state.signer != signer {
        return Err(Unregistered { vault_signer: latest.state.signer, superseded: false }.into());
    }
    Ok(latest)
}

/// The vault's latest and finalized views, validated, each naming `signer`, unretired.
/// Error cases:
/// - a read or its validation fails, or the guardian retired the signer
/// - with `unfinal`, the latest view's signer or signer epoch is not the finalized one's yet
///   (`EpochUnfinal`), whatever key each names: a promotion's is always a new key
/// - either view names another signer (`Unregistered`)
async fn views<C: CheckpointChain>(
    chain: &C,
    signer: AccountId,
    unfinal: bool,
) -> anyhow::Result<(Observation, Observation)> {
    let latest = chain.observe(View::Latest, &[]).await?;
    let finalized = chain.observe(View::Finalized, &[]).await?;
    latest.validate(&[])?;
    finalized.validate(&[])?;
    // A running node rides out a retired signer; a starting one cannot authenticate the vault's
    // authority until the timelock installs a signer.
    anyhow::ensure!(
        !latest.state.signer_retired() && !finalized.state.signer_retired(),
        "the vault's guardian retired its signer; the node starts once the vault's timelock installs one"
    );
    let epochs =
        latest.lifecycle.as_ref().zip(finalized.lifecycle.as_ref()).map(|(l, f)| (l.signer_epoch, f.signer_epoch));
    if unfinal && (latest.state.signer != finalized.state.signer || epochs.is_some_and(|(l, f)| l != f)) {
        return Err(anyhow::Error::new(EpochUnfinal));
    }
    let superseded = finalized.state.signer == signer && latest.state.signer != signer;
    for view in [&latest, &finalized] {
        if view.state.signer != signer {
            return Err(Unregistered { vault_signer: view.state.signer, superseded }.into());
        }
    }
    Ok((latest, finalized))
}

/// Whether both views are in the same signer epoch.
fn same_epoch(latest: &Observation, finalized: &Observation) -> bool {
    latest.lifecycle.as_ref().map(|evidence| evidence.signer_epoch)
        == finalized.lifecycle.as_ref().map(|evidence| evidence.signer_epoch)
}

/// Connects the checkpoint endpoint and reads its rollback anchor, and the registry's operator at
/// the same finalized block, before log recovery.
/// Error cases:
/// - configuration, transport, or vault reads fail
/// - the vault names another signer (`Unregistered`)
pub(crate) async fn connect(
    cfg: &NodeConfig,
    key: &Arc<EnclaveKey>,
    metrics: &Metrics,
) -> anyhow::Result<Option<Setup>> {
    connect_with(cfg, AccountId(key.address()), metrics, Start::Quiescent).await
}

/// `connect` for a member, the enclave whose address is `signer`, before the handover brings it the
/// key: at the vault's finalized checkpoint, with no wait for the incumbent's latest checkpoint to
/// finalize (`at_finality`).
/// Error cases:
/// - see `connect`
pub(crate) async fn connect_successor(
    cfg: &NodeConfig,
    signer: AccountId,
    metrics: &Metrics,
) -> anyhow::Result<Option<Setup>> {
    connect_with(cfg, signer, metrics, Start::Successor).await
}

/// `connect` for a member a promotion made the vault's signer (#595), before the promotion is final:
/// at the vault's latest block, which names `signer`, whose checkpoint is the one the promotion fixed
/// as its latest accepted, since no other key can post one after it. The poster it builds runs only
/// once the promotion is final (`await_signer_final`).
/// Error cases:
/// - see `connect`
pub(crate) async fn connect_promoted(
    cfg: &NodeConfig,
    signer: AccountId,
    metrics: &Metrics,
) -> anyhow::Result<Option<Setup>> {
    connect_with(cfg, signer, metrics, Start::Promoted).await
}

/// Waits until the vault's finalized view names `signer`, as it does once a promotion that made it
/// the signer is final, reading every `retry`.
/// Error cases:
/// - none: a failed read is retried
pub(crate) async fn await_signer_final<C: CheckpointChain>(chain: &C, signer: AccountId, retry: Duration) {
    loop {
        match chain.observe(View::Finalized, &[]).await {
            Ok(finalized) if finalized.state.signer == signer => return,
            Ok(_) => tracing::debug!("the vault's finalized view does not name this key yet"),
            Err(e) => tracing::debug!("reading the vault's finalized view failed: {e:#}"),
        }
        tokio::time::sleep(retry).await;
    }
}

/// Which view a start's checkpoint setup reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Start {
    /// A restart: the finalized view once the latest has nothing in flight.
    Quiescent,
    /// A member taking its keys over: the finalized view as it is.
    Successor,
    /// A promoted member: the latest view.
    Promoted,
}

/// The finalized view a start authenticates its journal's earlier work against and reads the
/// registry's operator at: `state`, or a promoted start's own read, its `state` being the latest view.
/// Error cases:
/// - the read fails
async fn authority<C: CheckpointChain>(chain: &C, start: Start, state: &Observation) -> anyhow::Result<Observation> {
    match start {
        Start::Promoted => chain.observe(View::Finalized, &[]).await,
        Start::Quiescent | Start::Successor => Ok(state.clone()),
    }
}

/// `connect` for `signer`, at the view `start` reads: a restart must not sign from a finalized parent
/// while its own latest inclusion is pending.
async fn connect_with(
    cfg: &NodeConfig,
    signer: AccountId,
    metrics: &Metrics,
    start: Start,
) -> anyhow::Result<Option<Setup>> {
    let Some(checkpoints) = &cfg.checkpoints else { return Ok(None) };
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let token = cfg.custody.evm.as_ref().context("checkpoint custody is not EVM")?.usdc;
    let evm = cfg.custody.evm.as_ref().context("checkpoint custody is not EVM")?;
    let (release_tag, release_depth) = release_policy(evm);
    let chain = CheckpointRpc::new(checkpoints, domain, token, release_tag, release_depth, &cfg.node.log_dir, signer)?;
    let chain = if checkpoints.relayer_gas.is_some() {
        chain.with_balance_quorum(BalanceQuorum::connect(&cfg.custody)?)
    } else {
        chain
    };
    let state = match start {
        Start::Quiescent => await_finality_boundary(&chain, signer, checkpoints.retry_interval, evm).await?,
        Start::Successor => at_finality(&chain, signer).await?,
        Start::Promoted => at_latest(&chain, signer).await?,
    };
    let finalized = authority(&chain, start, &state).await?;
    chain.authenticate_reservation(&finalized, start == Start::Promoted).await?;
    let registry_operator = chain.registry_operator_at(&finalized).await?;
    let parent = match &state.state.checkpoint {
        Some((seq, root)) => crate::log::CheckpointParent { has_checkpoint: true, seq: *seq, root: root.parse()? },
        None => Default::default(),
    };
    let posted_seq = parent.has_checkpoint.then_some(parent.seq);
    let poster = Poster {
        chain,
        registry: Some(Default::default()),
        domain,
        signer,
        retry: checkpoints.retry_interval,
        pending_deposits_critical_after: evm.stall_critical_after,
        post_withheld: Default::default(),
        surplus_raised: Default::default(),
        relayer_gas: checkpoints.relayer_gas.as_ref().map(|gas| RelayerGas {
            relayer: checkpoints.relayer,
            min_balance: gas.min_balance,
            balance_eth: metrics.checkpoint_relayer_balance_eth.clone(),
            low: Default::default(),
        }),
    };
    let (tx, rx) = mpsc::channel(64);
    Ok(Some(Setup {
        poster,
        tx,
        rx,
        posted_seq,
        parent,
        approvals: Default::default(),
        observation: state,
        registry_operator,
    }))
}

#[cfg(test)]
mod tests;
