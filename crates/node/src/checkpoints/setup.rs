//! Checkpoint setup before the node opens its log for writing.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use engine_core::types::AccountId;
use tokio::sync::mpsc;

use crate::checkpoints::{ApprovalRegistry, CheckpointChain, CheckpointRpc, Observation, Poster, View, Work};
use crate::config::{EvmCustodySection, FinalityTag, NodeConfig};
use crate::log::CheckpointDomain;
use crate::tee::EnclaveKey;

pub(crate) struct Setup {
    pub poster: Poster<CheckpointRpc>,
    pub tx: mpsc::Sender<Work>,
    pub rx: mpsc::Receiver<Work>,
    pub approvals: ApprovalRegistry,
    pub observation: Observation,
    pub parent: crate::log::CheckpointParent,
    pub posted_seq: Option<u64>,
}

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
            let latest = chain.observe(View::Latest, &[]).await?;
            let finalized = chain.observe(View::Finalized, &[]).await?;
            latest.validate(&[])?;
            finalized.validate(&[])?;
            // A running node rides out a retired signer; a starting one cannot authenticate the
            // vault's authority until the timelock installs a signer.
            anyhow::ensure!(
                !latest.state.signer_retired() && !finalized.state.signer_retired(),
                "the vault's guardian retired its signer; the node starts once the vault's timelock installs one"
            );
            anyhow::ensure!(
                latest.state.signer == signer && finalized.state.signer == signer,
                "vault signer differs from this enclave; signer recovery is required"
            );
            if latest.state.checkpoint == finalized.state.checkpoint
                && latest.lifecycle.as_ref().map(|evidence| evidence.signer_epoch)
                    == finalized.lifecycle.as_ref().map(|evidence| evidence.signer_epoch)
            {
                break anyhow::Ok(finalized);
            }
            tokio::time::sleep(retry).await;
        }
    })
    .await
    .context("checkpoint startup awaits custody finality beyond its finality budget")?
}

/// Connects the checkpoint endpoint and reads its rollback anchor before log recovery.
/// Error cases:
/// - configuration, transport, or vault reads fail
pub(crate) async fn connect(cfg: &NodeConfig, key: &Arc<EnclaveKey>) -> anyhow::Result<Option<Setup>> {
    let Some(checkpoints) = &cfg.checkpoints else { return Ok(None) };
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let token = cfg.custody.evm.as_ref().context("checkpoint custody is not EVM")?.usdc;
    let evm = cfg.custody.evm.as_ref().context("checkpoint custody is not EVM")?;
    let (release_tag, release_depth) = release_policy(evm);
    let chain = CheckpointRpc::new(
        checkpoints,
        domain,
        token,
        release_tag,
        release_depth,
        &cfg.node.log_dir,
        AccountId(key.address()),
    )?;
    let state = await_finality_boundary(&chain, AccountId(key.address()), checkpoints.retry_interval, evm).await?;
    chain.authenticate_reservation(&state).await?;
    let parent = match &state.state.checkpoint {
        Some((seq, root)) => crate::log::CheckpointParent { has_checkpoint: true, seq: *seq, root: root.parse()? },
        None => Default::default(),
    };
    let posted_seq = parent.has_checkpoint.then_some(parent.seq);
    let poster = Poster {
        chain,
        registry: Some(Default::default()),
        domain,
        signer: AccountId(key.address()),
        retry: checkpoints.retry_interval,
        pending_deposits_critical_after: evm.stall_critical_after,
        post_withheld: Default::default(),
    };
    let (tx, rx) = mpsc::channel(64);
    Ok(Some(Setup { poster, tx, rx, posted_seq, parent, approvals: Default::default(), observation: state }))
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

    #[tokio::test(start_paused = true)]
    async fn startup_refuses_a_vault_whose_guardian_retired_its_signer() {
        let chain = LaggingChain::new(AccountId([0; 20]), 0);
        let err = await_finality_boundary(&chain, AccountId([0xaa; 20]), Duration::from_secs(1), &evm("1s", "30s"))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("guardian retired its signer"), "{err:#}");
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
