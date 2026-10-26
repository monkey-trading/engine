//! Checkpoint reconciliation with staker distributions paid: the compensated comparison, the
//! overpaid ceiling, and withholding checkpoints while collateral is diverged.

use std::time::Duration;

use engine_core::Fixed;
use engine_core::types::AccountId;
use tokio::sync::mpsc;

use super::poster;
use crate::checkpoints::Work;
use crate::log::Checkpoint;
use crate::sequencer::Request;

#[tokio::test(start_paused = true)]
async fn paid_distributions_compensate_the_pending_deposit_classification() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let (poster, mut work, _key) = poster();
    // Stakers have claimed 40 of the 100 the ledger credits, and the vault counts one deposit the
    // checkpoint has not credited yet: the compensated balance is back at the ledger, so the
    // in-flight deposit keeps payouts running instead of reading as a collateral mismatch.
    work.distributed = engine_core::UsdcUnits(40_000_000);
    {
        let mut state = poster.chain.state.lock().unwrap();
        state.distributions_paid = Fixed::from_int(40);
        state.balance = work.liabilities - Fixed::from_int(40);
        state.deposits += 1;
    }
    let (work_tx, work_rx) = mpsc::channel(4);
    let (tx, mut rx) = mpsc::channel(4);
    work_tx.send(work).await.unwrap();
    let task = tokio::spawn(poster.run(work_rx, tx));
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { healthy: true, .. })));
    let before: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    assert!(!before.iter().any(|alert| alert["alert"] == "collateral_mismatch"), "{before:?}");
    // The deposit never credits: past the catch-up deadline the escalation pauses payouts.
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { healthy: false, .. })));
    assert!(std::iter::from_fn(|| alerts.try_recv().ok()).any(|alert| alert["alert"] == "collateral_mismatch"));
    drop(work_tx);
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn paid_distributions_reconcile_healthy_when_the_balance_compensates() {
    let (poster, mut work, _key) = poster();
    // Stakers claimed 40 of the 100 the ledger credits; the vault's balance is short by exactly
    // that, so the compensated comparison reconciles.
    work.distributed = engine_core::UsdcUnits(40_000_000);
    {
        let mut state = poster.chain.state.lock().unwrap();
        state.distributions_paid = Fixed::from_int(40);
        state.balance = work.liabilities - Fixed::from_int(40);
    }
    let (work_tx, work_rx) = mpsc::channel(4);
    let (tx, mut rx) = mpsc::channel(4);
    work_tx.send(work).await.unwrap();
    let task = tokio::spawn(poster.run(work_rx, tx));
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { healthy: true, .. })));
    drop(work_tx);
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn distributions_paid_past_the_engines_allocation_is_a_mismatch() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let (poster, mut work, key) = poster();
    let posts = poster.chain.posts.clone();
    // The vault has paid 50 USDC in distributions but the engine allocated only 40: the balance
    // still compensates to the ledger, and the overpayment alone is the mismatch.
    work.distributed = engine_core::UsdcUnits(40_000_000);
    {
        let mut state = poster.chain.state.lock().unwrap();
        state.distributions_paid = Fixed::from_int(50);
        state.balance = work.liabilities - Fixed::from_int(50);
    }
    let (work_tx, work_rx) = mpsc::channel(4);
    let (tx, mut rx) = mpsc::channel(4);
    work_tx.send(work).await.unwrap();
    let task = tokio::spawn(poster.run(work_rx, tx));
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { healthy: false, .. })));
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { healthy: false, .. })));
    assert!(std::iter::from_fn(|| alerts.try_recv().ok()).any(|alert| alert["alert"] == "collateral_mismatch"));
    // A diverged verdict withholds every later checkpoint: only the first post went out.
    let parent = crate::log::CheckpointParent { has_checkpoint: true, seq: 5, root: [3; 32].into() };
    let domain = crate::log::CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
    let second = Work {
        checkpoint: Checkpoint {
            parent: Some(parent),
            seq: 6,
            root: format!("0x{}", hex::encode([4; 32])),
            signer: key.address_hex(),
            signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, 6, &[4; 32], parent),
            domain: Some(domain),
            deferred_withdrawals: true,
        },
        liabilities: Fixed::from_int(100),
        deposits: 1,
        distributed: engine_core::UsdcUnits(40_000_000),
        authorized: Fixed::ZERO,
        coverage: crate::checkpoints::DepositCoverage::Historical,
    };
    work_tx.send(second).await.unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(*posts.lock().unwrap(), 1, "no checkpoint posts while reconciliation is diverged");
    drop(work_tx);
    task.await.unwrap().unwrap();
}
