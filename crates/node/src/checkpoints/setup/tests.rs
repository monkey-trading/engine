use std::sync::Mutex;

use alloy::primitives::B256;
use engine_core::Fixed;

use super::*;
use crate::checkpoints::{ApprovalId, LifecycleEvidence, VaultState};

#[test]
fn latest_credit_and_its_confirmation_depth_cannot_weaken_checkpoint_release() {
    let cfg: NodeConfig = toml::from_str(include_str!("../../../../../config/anvil.toml")).unwrap();
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
                let checkpoint = (*reads >= self.release_after).then(|| self.latest.state.checkpoint.clone().unwrap());
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

/// A successor's start waits, by the error's type a member's take waits on, while the latest view
/// has a promotion's new key in a new epoch the finalized view does not show yet.
#[tokio::test(start_paused = true)]
async fn a_successor_waits_for_a_signer_epoch_not_final_yet() {
    let signer = AccountId([0xaa; 20]);
    let mut chain = LaggingChain::new(signer, 0);
    let promote = |chain: &mut LaggingChain, (key, epoch): (AccountId, u64)| {
        let lifecycle = chain.latest.lifecycle.as_mut().expect("a lifecycle");
        (chain.latest.state.signer, lifecycle.signer, lifecycle.signer_epoch) = (key, key, epoch);
    };
    promote(&mut chain, (AccountId([0xbb; 20]), 2));
    let unfinal = at_finality(&chain, signer).await.unwrap_err();
    assert!(
        unfinal.downcast_ref::<EpochUnfinal>().is_some(),
        "an unfinal promotion read as another failure: {unfinal:#}"
    );
    promote(&mut chain, (signer, 1));
    assert!(at_finality(&chain, signer).await.is_ok(), "a final epoch still waits");
}

/// A promoted start's view is the latest one, which historical authority refuses: it authenticates its
/// journal's earlier work, and reads the registry's operator, at a finalized view of its own.
#[tokio::test(start_paused = true)]
async fn a_promoted_start_reads_historical_authority_at_finality() {
    let chain = LaggingChain::new(AccountId([0xaa; 20]), 0);
    let latest = chain.observe(View::Latest, &[]).await.unwrap();
    assert_eq!(authority(&chain, Start::Promoted, &latest).await.unwrap().view, View::Finalized);
    let finalized = chain.observe(View::Finalized, &[]).await.unwrap();
    for start in [Start::Quiescent, Start::Successor] {
        assert_eq!(authority(&chain, start, &finalized).await.unwrap().view, View::Finalized, "{start:?}");
    }
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
