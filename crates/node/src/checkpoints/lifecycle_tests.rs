//! Paid collateral, bounded nonce evidence, and independently authenticated signer rotation.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::B256;
use engine_core::Fixed;
use engine_core::types::AccountId;
use tokio::sync::mpsc;

use crate::checkpoints::{
    APPROVAL_PAGE_SIZE, AcceptedCheckpoint, ApprovalId, ApprovalStatus, CheckpointChain, LifecycleEvidence,
    Observation, Poster, VaultState, View, Work,
};
use crate::log::{Checkpoint, CheckpointDomain};
use crate::sequencer::Request;
use crate::tee::EnclaveKey;

#[derive(Clone)]
struct Chain {
    latest: Arc<Mutex<Observation>>,
    finalized: Arc<Mutex<Observation>>,
    accepted: Arc<Mutex<BTreeMap<u64, AcceptedCheckpoint>>>,
    claimed: Arc<Mutex<Vec<(AccountId, u64)>>>,
    pages: Arc<Mutex<Vec<Vec<ApprovalId>>>>,
}

impl CheckpointChain for Chain {
    async fn state(&self, view: View) -> anyhow::Result<VaultState> {
        Ok(self.observe(view, &[]).await?.state)
    }

    async fn observe(&self, view: View, approvals: &[ApprovalId]) -> anyhow::Result<Observation> {
        assert!(approvals.len() <= APPROVAL_PAGE_SIZE);
        let mut at = match view {
            View::Latest => self.latest.lock().unwrap().clone(),
            View::Finalized => self.finalized.lock().unwrap().clone(),
        };
        if view == View::Finalized {
            self.pages.lock().unwrap().push(approvals.to_vec());
        }
        at.view = view;
        at.lifecycle.as_mut().unwrap().approvals = approvals
            .iter()
            .map(|approval| ApprovalStatus {
                approval: *approval,
                claimed: self.claimed.lock().unwrap().contains(&(approval.account, approval.nonce)),
            })
            .collect();
        Ok(at)
    }

    async fn accepted_checkpoints(
        &self,
        _: &Observation,
        sequences: &[u64],
    ) -> anyhow::Result<Vec<AcceptedCheckpoint>> {
        sequences
            .iter()
            .map(|seq| {
                self.accepted
                    .lock()
                    .unwrap()
                    .get(seq)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("checkpoint is not accepted"))
            })
            .collect()
    }

    async fn post(&self, _: &Checkpoint) -> anyhow::Result<()> {
        panic!("test checkpoints are already accepted")
    }
}

#[tokio::test]
async fn payouts_reconcile_but_arbitrary_burns_never_heal_as_payments() {
    let (poster, work) = fixture();
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), true));
    {
        let mut at = poster.chain.finalized.lock().unwrap();
        at.state.balance -= Fixed::from_int(3);
    }
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), false), "a burn is not a payment");
    poster.chain.finalized.lock().unwrap().lifecycle.as_mut().unwrap().total_withdrawn = Fixed::from_int(3);
    assert_eq!(
        poster.reconcile(&work).await.unwrap(),
        (Some(5), true),
        "authenticated payments reduce expected custody"
    );
    {
        let mut at = poster.chain.finalized.lock().unwrap();
        at.state.balance -= Fixed::from_int(1);
    }
    assert_eq!(
        poster.reconcile(&work).await.unwrap(),
        (Some(5), false),
        "a burn in addition to real payments stays a mismatch"
    );
}

#[tokio::test]
async fn paid_total_cannot_exceed_checkpoint_authorized_approvals_even_when_balance_matches() {
    let (poster, work) = fixture();
    for paid in [Fixed::from_int(11), Fixed::from_raw(-1)] {
        {
            let mut at = poster.chain.finalized.lock().unwrap();
            at.state.balance = work.liabilities - paid;
            at.lifecycle.as_mut().unwrap().total_withdrawn = paid;
        }
        assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), false), "invalid paid boundary {paid}");
    }
}

#[tokio::test]
async fn latest_payment_and_nonce_use_never_replace_finalized_evidence() {
    let (poster, work) = fixture();
    let approval = approval(1);
    poster.chain.latest.lock().unwrap().lifecycle.as_mut().unwrap().total_withdrawn = Fixed::from_int(4);
    let (_, healthy, lifecycle) = poster.reconcile_lifecycle(&work, &[&work], &[approval]).await.unwrap();
    assert!(healthy);
    let lifecycle = lifecycle.unwrap();
    assert_eq!(lifecycle.block_hash, B256::repeat_byte(8));
    assert_eq!(lifecycle.total_withdrawn, Fixed::ZERO);
    assert_eq!(lifecycle.approvals, [ApprovalStatus { approval, claimed: false }]);
}

#[tokio::test]
async fn unrelated_paid_nonce_does_not_claim_another_approval() {
    let (poster, work) = fixture();
    let wanted = approval(1);
    poster.chain.claimed.lock().unwrap().push((wanted.account, wanted.nonce + 1));
    let (_, _, lifecycle) = poster.reconcile_lifecycle(&work, &[&work], &[wanted]).await.unwrap();
    assert_eq!(lifecycle.unwrap().approvals, [ApprovalStatus { approval: wanted, claimed: false }]);
    poster.chain.claimed.lock().unwrap().push((wanted.account, wanted.nonce));
    let (_, _, lifecycle) = poster.reconcile_lifecycle(&work, &[&work], &[wanted]).await.unwrap();
    assert_eq!(lifecycle.unwrap().approvals, [ApprovalStatus { approval: wanted, claimed: true }]);
}

#[tokio::test]
async fn an_unfinalized_epoch_change_never_authorizes_release_even_with_same_signer() {
    let (poster, work) = fixture();
    poster.chain.latest.lock().unwrap().lifecycle.as_mut().unwrap().signer_epoch = 2;
    assert!(
        poster.reconcile_lifecycle(&work, &[&work], &[approval(1)]).await.unwrap_err().to_string().contains("epoch")
    );
}

#[tokio::test]
async fn rotated_signer_can_recover_an_accepted_old_anchor_without_authorizing_unaccepted_old_roots() {
    let (mut poster, work) = fixture();
    let new = AccountId(EnclaveKey::generate().address());
    poster.signer = new;
    for view in [&poster.chain.latest, &poster.chain.finalized] {
        let mut at = view.lock().unwrap();
        at.state.signer = new;
        let lifecycle = at.lifecycle.as_mut().unwrap();
        lifecycle.signer = new;
        lifecycle.signer_epoch = 2;
    }
    poster.startup(Some(&work.checkpoint)).await.unwrap();
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), true));
    let mut forged = work.checkpoint.clone();
    forged.signer = format!("{new}");
    assert!(poster.startup(Some(&forged)).await.is_err(), "self-named signer is insufficient");
    poster.chain.accepted.lock().unwrap().get_mut(&5).unwrap().signer = AccountId([9; 20]);
    assert!(poster.startup(Some(&work.checkpoint)).await.is_err());
    assert!(poster.reconcile(&work).await.unwrap_err().is::<crate::checkpoints::Conflict>());
}

#[tokio::test]
async fn finalized_rollback_and_competing_fork_never_supply_lifecycle_evidence() {
    let (poster, work) = fixture();
    poster.chain.finalized.lock().unwrap().state.checkpoint = None;
    poster.chain.finalized.lock().unwrap().lifecycle.as_mut().unwrap().checkpoint_seq = None;
    poster.chain.finalized.lock().unwrap().lifecycle.as_mut().unwrap().checkpoint_root = None;
    assert_eq!(poster.reconcile_lifecycle(&work, &[&work], &[approval(1)]).await.unwrap(), (None, false, None));
    let mut fork = poster.chain.latest.lock().unwrap().clone();
    fork.state.checkpoint.as_mut().unwrap().1 = format!("0x{}", "ff".repeat(32));
    fork.lifecycle.as_mut().unwrap().checkpoint_root = fork.state.checkpoint.as_ref().map(|value| value.1.clone());
    *poster.chain.finalized.lock().unwrap() = fork;
    assert!(
        poster
            .reconcile_lifecycle(&work, &[&work], &[approval(1)])
            .await
            .unwrap_err()
            .is::<crate::checkpoints::Conflict>()
    );
}

#[tokio::test(start_paused = true)]
async fn registry_polling_is_bounded_rotates_pages_and_retires_only_matching_claims() {
    let (poster, work) = fixture();
    let chain = poster.chain.clone();
    let approvals: crate::checkpoints::ApprovalRegistry = Default::default();
    for nonce in 0..(APPROVAL_PAGE_SIZE as u64 + 3) {
        let id = approval(nonce);
        approvals.lock().unwrap().insert((id.account, id.nonce), id);
    }
    let paid = approval(1);
    chain.claimed.lock().unwrap().push((paid.account, paid.nonce));
    let (work_tx, work_rx) = mpsc::channel(4);
    let (status_tx, mut status_rx) = mpsc::channel(4);
    work_tx.send(work).await.unwrap();
    let task = tokio::spawn(poster.run_with_approvals(work_rx, status_tx, Vec::new(), approvals.clone()));
    let first = match status_rx.recv().await.unwrap() {
        Request::CheckpointStatus { healthy: true, lifecycle: Some(evidence), .. } => evidence,
        _ => panic!("missing finalized lifecycle evidence"),
    };
    assert_eq!(first.approvals.len(), APPROVAL_PAGE_SIZE);
    assert!(first.approvals.iter().any(|status| status.approval == paid && status.claimed));
    assert!(!approvals.lock().unwrap().contains_key(&(paid.account, paid.nonce)));
    let second = match status_rx.recv().await.unwrap() {
        Request::CheckpointStatus { healthy: true, lifecycle: Some(evidence), .. } => evidence,
        _ => panic!("missing second lifecycle page"),
    };
    assert_eq!(second.approvals.len(), 3);
    assert_eq!(second.approvals[0].approval.nonce, APPROVAL_PAGE_SIZE as u64);
    assert_eq!(approvals.lock().unwrap().len(), APPROVAL_PAGE_SIZE + 2);
    let third = match status_rx.recv().await.unwrap() {
        Request::CheckpointStatus { healthy: true, lifecycle: Some(evidence), .. } => evidence,
        _ => panic!("missing wrapped lifecycle page"),
    };
    assert_eq!(third.approvals.len(), APPROVAL_PAGE_SIZE);
    assert_eq!(third.approvals[0].approval.nonce, 0);
    assert!(third.approvals.iter().all(|status| status.approval != paid));
    drop(work_tx);
    task.await.unwrap().unwrap();
    assert!(chain.pages.lock().unwrap().iter().all(|page| page.len() <= APPROVAL_PAGE_SIZE));
}

#[test]
fn inconsistent_lifecycle_metadata_and_changed_query_identity_are_rejected() {
    let (poster, _) = fixture();
    let at = poster.chain.finalized.lock().unwrap().clone();
    for field in 0..6 {
        let mut bad = at.clone();
        let evidence = bad.lifecycle.as_mut().unwrap();
        match field {
            0 => evidence.height += 1,
            1 => evidence.signer = AccountId([7; 20]),
            2 => evidence.signer_epoch = 0,
            3 => evidence.checkpoint_seq = Some(9),
            4 => evidence.checkpoint_root = None,
            _ => evidence.approvals.push(ApprovalStatus { approval: approval(1), claimed: true }),
        }
        assert!(bad.validate(&[]).is_err(), "field {field}");
    }
    let mut changed = at;
    changed.lifecycle.as_mut().unwrap().approvals.push(ApprovalStatus { approval: approval(2), claimed: false });
    assert!(changed.validate(&[approval(1)]).is_err());
}

// ── Helpers ──

fn approval(nonce: u64) -> ApprovalId {
    ApprovalId { account: AccountId([1; 20]), nonce, approved_seq: 4 }
}

fn fixture() -> (Poster<Chain>, Work) {
    let key = EnclaveKey::test_key();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
    let root = [3; 32];
    let parent = Default::default();
    let checkpoint = Checkpoint {
        seq: 5,
        root: format!("0x{}", hex::encode(root)),
        signer: key.address_hex(),
        signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, 5, &root, parent),
        domain: Some(domain),
        parent: Some(parent),
        deferred_withdrawals: true,
    };
    let state = VaultState {
        height: 8,
        signer: AccountId(key.address()),
        checkpoint: Some((5, checkpoint.root.clone())),
        balance: Fixed::from_int(100),
        deposits: 1,
    };
    let lifecycle = LifecycleEvidence {
        height: 8,
        block_hash: B256::repeat_byte(8),
        signer: state.signer,
        signer_epoch: 1,
        total_withdrawn: Fixed::ZERO,
        checkpoint_seq: Some(5),
        checkpoint_root: Some(checkpoint.root.clone()),
        approvals: Vec::new(),
    };
    let at = Observation { view: View::Finalized, state, lifecycle: Some(lifecycle) };
    let chain = Chain {
        latest: Arc::new(Mutex::new(at.clone())),
        finalized: Arc::new(Mutex::new(at)),
        accepted: Arc::new(Mutex::new(BTreeMap::from([(
            5,
            AcceptedCheckpoint {
                seq: 5,
                root: checkpoint.root.clone(),
                signer: AccountId(key.address()),
                signer_epoch: 1,
            },
        )]))),
        claimed: Default::default(),
        pages: Default::default(),
    };
    let work = Work { checkpoint, liabilities: Fixed::from_int(100), authorized: Fixed::from_int(10), deposits: 1 };
    (
        Poster {
            chain,
            registry: None,
            domain,
            signer: AccountId(key.address()),
            retry: Duration::from_millis(1),
            pending_deposits_critical_after: Duration::from_secs(1),
        },
        work,
    )
}
