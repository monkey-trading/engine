//! Captured coverage component contracts; actual V20 Work capture is tested after activation.
use super::*;
use crate::checkpoints::DepositCoverage;

#[tokio::test]
async fn equal_balance_and_count_cannot_prove_an_unscanned_receipt_prefix() {
    let (poster, mut work) = poster();
    work.coverage = DepositCoverage::AwaitingCanonicalScan;
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(work.checkpoint.seq), false));
    assert_eq!(*poster.chain.posts.lock().unwrap(), 1);
    assert_eq!(work.coverage, DepositCoverage::AwaitingCanonicalScan);
}

#[tokio::test]
async fn newer_work_cannot_lend_receipt_coverage_to_an_older_finalized_checkpoint() {
    let (mut poster, mut first) = poster();
    let key = EnclaveKey::test_key();
    first.checkpoint = checkpoint(&key, 5, [3; 32]);
    first.coverage = DepositCoverage::AwaitingCanonicalScan;
    let parent = crate::log::CheckpointParent {
        has_checkpoint: true,
        seq: first.checkpoint.seq,
        root: first.checkpoint.root.parse().unwrap(),
    };
    let mut second = first.clone();
    second.checkpoint = checkpoint(&key, 6, [4; 32]);
    second.checkpoint.parent = Some(parent);
    let domain = second.checkpoint.domain.unwrap();
    second.checkpoint.signature = key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, 6, &[4; 32], parent);
    second.liabilities += Fixed::from_int(50);
    second.deposits = 2;
    second.coverage = DepositCoverage::Canonical;
    poster.signer = AccountId(key.address());
    *poster.chain.state.lock().unwrap() = VaultState {
        height: 2,
        signer: poster.signer,
        checkpoint: Some((6, second.checkpoint.root.clone())),
        balance: second.liabilities,
        deposits: second.deposits,
        deposited: second.liabilities,
    };
    *poster.chain.through.lock().unwrap() = vec![first.liabilities, second.liabilities];
    let finalized = Arc::new(Mutex::new(VaultState {
        height: 1,
        signer: poster.signer,
        checkpoint: Some((5, first.checkpoint.root.clone())),
        balance: first.liabilities,
        deposits: first.deposits,
        deposited: first.liabilities,
    }));
    poster.chain.finalized = Some(finalized.clone());
    assert_eq!(poster.reconcile_known(&second, &[&first, &second]).await.unwrap(), (Some(5), false));
    assert_eq!(first.coverage, DepositCoverage::AwaitingCanonicalScan);
    *finalized.lock().unwrap() = poster.chain.state.lock().unwrap().clone();
    assert_eq!(poster.reconcile_known(&second, &[&first, &second]).await.unwrap(), (Some(6), true));
    assert_eq!(*poster.chain.posts.lock().unwrap(), 0, "known public relay must not repost either prefix");
}
