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
            let (latest, finalized) = views(chain, signer).await?;
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
/// - see `views`, or a change of signer is not yet finalized
async fn at_finality<C: CheckpointChain>(chain: &C, signer: AccountId) -> anyhow::Result<Observation> {
    let (latest, finalized) = views(chain, signer).await?;
    let epochs = latest.lifecycle.as_ref().zip(finalized.lifecycle.as_ref());
    anyhow::ensure!(
        epochs.is_none_or(|(latest, finalized)| latest.signer_epoch == finalized.signer_epoch),
        "the vault's signer epoch is not yet finalized"
    );
    Ok(finalized)
}

/// The vault's latest view, validated, naming `signer`, unretired.
/// Error cases:
/// - the read or its validation fails, the guardian retired the signer, or the view names another
///   signer (`Unregistered`)
async fn at_latest<C: CheckpointChain>(chain: &C, signer: AccountId) -> anyhow::Result<Observation> {
    let latest = chain.observe(View::Latest, &[]).await?;
    latest.validate(&[])?;
    anyhow::ensure!(!latest.state.signer_retired(), "the vault's guardian retired its signer");
    if latest.state.signer != signer {
        return Err(Unregistered { vault_signer: latest.state.signer }.into());
    }
    Ok(latest)
}

/// The vault's latest and finalized views, validated, each naming `signer`, unretired.
/// Error cases:
/// - a read or its validation fails, the guardian retired the signer, or either view names another
///   signer (`Unregistered`)
async fn views<C: CheckpointChain>(chain: &C, signer: AccountId) -> anyhow::Result<(Observation, Observation)> {
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
    for view in [&latest, &finalized] {
        if view.state.signer != signer {
            return Err(Unregistered { vault_signer: view.state.signer }.into());
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
    chain.authenticate_reservation(&state).await?;
    // The registry's operator is read at finality: a promoted start's latest view is not finalized.
    let registry_operator = match start {
        Start::Promoted => chain.registry_operator_at(&chain.observe(View::Finalized, &[]).await?).await?,
        Start::Quiescent | Start::Successor => chain.registry_operator_at(&state).await?,
    };
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
mod tests {
    use std::sync::Mutex;

    use alloy::primitives::B256;
    use engine_core::Fixed;

    use super::*;
    use crate::checkpoints::{ApprovalId, LifecycleEvidence, VaultState};

    #[test]
    fn latest_credit_and_its_confirmation_depth_cannot_weaken_checkpoint_release() {
        let cfg: NodeConfig = toml::from_str(include_str!("../../../../config/anvil.toml")).unwrap();
        let mut evm = cfg.custody.evm.unwrap();
        evm.withdrawal_finality_tag = FinalityTag::Finalized;
        for tag in [FinalityTag::Latest, FinalityTag::Safe, FinalityTag::Finalized] {
            evm.finality_tag = tag;
            evm.confirmations = 37;
            assert_eq!(release_policy(&evm), (FinalityTag::Finalized, 0));
        }
    }

    /// The finalized view trails the latest checkpoint for a set number of reads, as a release
    /// tag lagging a moving head does.
    struct LaggingChain {
        signer: AccountId,
        latest: Observation,
        finalized_reads: Mutex<usize>,
        /// The finalized checkpoint reaches the latest's after this many reads.
        release_after: usize,
    }

    impl LaggingChain {
        fn observation(&self, view: View, checkpoint: Option<(u64, String)>, height: u64) -> Observation {
            Observation {
                view,
                state: VaultState {
                    height,
                    signer: self.signer,
                    checkpoint: checkpoint.clone(),
                    balance: Fixed::ZERO,
                    deposits: 0,
                    deposited: Fixed::ZERO,
                    distributions_paid: Fixed::ZERO,
                },
                lifecycle: checkpoint.map(|(seq, root)| LifecycleEvidence {
                    height,
                    block_hash: B256::repeat_byte(u8::try_from(height).unwrap()),
                    signer: self.signer,
                    signer_epoch: 1,
                    withdrawals_paused: false,
                    total_withdrawn: Fixed::ZERO,
                    checkpoint_seq: Some(seq),
                    checkpoint_root: Some(root),
                    approvals: Vec::new(),
                }),
            }
        }

        fn new(signer: AccountId, release_after: usize) -> Self {
            let checkpoint = Some((1, "root".to_owned()));
            LaggingChain {
                signer,
                latest: Observation {
                    view: View::Latest,
                    state: VaultState {
                        height: 10,
                        signer,
                        checkpoint,
                        balance: Fixed::ZERO,
                        deposits: 0,
                        deposited: Fixed::ZERO,
                        distributions_paid: Fixed::ZERO,
                    },
                    lifecycle: Some(LifecycleEvidence {
                        height: 10,
                        block_hash: B256::repeat_byte(10),
                        signer,
                        signer_epoch: 1,
                        withdrawals_paused: false,
                        total_withdrawn: Fixed::ZERO,
                        checkpoint_seq: Some(1),
                        checkpoint_root: Some("root".to_owned()),
                        approvals: Vec::new(),
                    }),
                },
                finalized_reads: Mutex::new(0),
                release_after,
            }
        }
    }

    impl CheckpointChain for LaggingChain {
        async fn state(&self, view: View) -> anyhow::Result<VaultState> {
            Ok(self.observe(view, &[]).await?.state)
        }

        async fn observe(&self, view: View, _: &[ApprovalId]) -> anyhow::Result<Observation> {
            match view {
                View::Latest => Ok(self.latest.clone()),
                View::Finalized => {
                    let mut reads = self.finalized_reads.lock().unwrap();
                    *reads += 1;
                    let checkpoint =
                        (*reads >= self.release_after).then(|| self.latest.state.checkpoint.clone().unwrap());
                    Ok(self.observation(view, checkpoint, 9))
                }
            }
        }

        async fn post(&self, _: &crate::log::Checkpoint) -> anyhow::Result<()> {
            panic!("test checkpoints are already accepted")
        }
    }

    fn evm(stall_critical_after: &str, finality_stalled_after: &str) -> EvmCustodySection {
        toml::from_str(&format!(
            r#"
            providers = [{{ name = "a", rpc_url = "http://127.0.0.1:1" }}]
            usdc = "0xc0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0"
            finality_tag = "latest"
            withdrawal_finality_tag = "finalized"
            confirmations = 0
            start_block = 0
            max_block_range = 1000
            poll_interval = "10ms"
            rpc_timeout = "1s"
            stall_critical_after = "{stall_critical_after}"
            finality_stalled_after = "{finality_stalled_after}"
            cursor_path = "cursor.json"
            "#
        ))
        .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn startup_waits_out_a_release_tag_past_the_batch_posting_budget() {
        let signer = AccountId([0xaa; 20]);
        // Batch posting lands every second; the release tag reaches the pending inclusion only
        // after fifteen seconds. Startup must hold past the 1s batch budget on the release
        // tag's own 30s budget.
        let chain = LaggingChain::new(signer, 15);
        let start = tokio::time::Instant::now();
        let state = await_finality_boundary(&chain, signer, Duration::from_secs(1), &evm("1s", "30s")).await.unwrap();
        assert_eq!(state.view, View::Finalized);
        assert_eq!(state.state.checkpoint, Some((1, "root".to_owned())));
        assert!(
            start.elapsed() >= Duration::from_secs(10),
            "the boundary arrived only through the release-tag retries: {:?}",
            start.elapsed()
        );
    }

    /// A successor reads the vault's finalized checkpoint at once while the incumbent's latest one is
    /// still in flight: the cold start's wait would hold it until the release tag caught up.
    #[tokio::test(start_paused = true)]
    async fn a_successor_reads_the_finalized_checkpoint_with_one_in_flight() {
        let signer = AccountId([0xaa; 20]);
        let chain = LaggingChain::new(signer, usize::MAX);
        let start = tokio::time::Instant::now();
        let state = at_finality(&chain, signer).await.unwrap();
        assert_eq!((state.view, state.state.checkpoint), (View::Finalized, None));
        assert!(start.elapsed() < Duration::from_secs(1), "it waited for the latest checkpoint to finalize");
        let other = at_finality(&chain, AccountId([0xbb; 20])).await.unwrap_err();
        assert!(other.downcast::<Unregistered>().is_ok(), "another signer's vault was read");
    }

    #[tokio::test(start_paused = true)]
    async fn startup_refuses_a_vault_whose_guardian_retired_its_signer() {
        let chain = LaggingChain::new(AccountId([0; 20]), 0);
        let err = await_finality_boundary(&chain, AccountId([0xaa; 20]), Duration::from_secs(1), &evm("1s", "30s"))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("guardian retired its signer"), "{err:#}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_vault_that_names_another_signer_is_reported_at_once_as_unregistered() {
        let vault_signer = AccountId([0xbb; 20]);
        let chain = LaggingChain::new(vault_signer, 1);
        let start = tokio::time::Instant::now();
        let err = await_finality_boundary(&chain, AccountId([0xaa; 20]), Duration::from_secs(1), &evm("1s", "30s"))
            .await
            .unwrap_err();
        let unregistered = err.downcast::<Unregistered>().expect("a typed refusal registration can wait on");
        assert_eq!(unregistered.vault_signer, vault_signer);
        assert!(start.elapsed() < Duration::from_secs(1), "it waited for finality first");
    }

    #[tokio::test(start_paused = true)]
    async fn startup_refuses_to_open_only_past_the_finality_budget() {
        let signer = AccountId([0xaa; 20]);
        // The release tag never reaches the pending inclusion.
        let chain = LaggingChain::new(signer, usize::MAX);
        let start = tokio::time::Instant::now();
        let err = await_finality_boundary(&chain, signer, Duration::from_secs(1), &evm("1s", "30s")).await.unwrap_err();
        assert!(err.to_string().contains("beyond its finality budget"), "{err:#}");
        assert!(
            start.elapsed() >= Duration::from_secs(30),
            "startup must wait out the release tag's own budget, not the batch budget: {:?}",
            start.elapsed()
        );
    }
}
