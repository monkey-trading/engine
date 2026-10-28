//! Checkpoint reconciliation with staker distributions paid: the compensated comparison, the
//! overpaid ceiling, and withholding checkpoints while collateral is diverged.

use std::time::Duration;

use engine_core::Fixed;
use engine_core::types::AccountId;
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::poster;
use crate::checkpoints::{Health, Work};
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
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { health: Some(Health::Healthy), .. })));
    let before: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    assert!(!before.iter().any(|alert| alert["alert"] == "collateral_mismatch"), "{before:?}");
    // The deposit never credits: past the catch-up deadline the escalation pauses payouts.
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { health: Some(Health::Pending), .. })));
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
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { health: Some(Health::Healthy), .. })));
    drop(work_tx);
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn distributions_paid_past_the_engines_allocation_is_a_mismatch() {
    let (_guard, captured) = crate::alerts::capture_until_dropped();
    let (poster, mut work, key) = poster();
    let posts = poster.chain.posts.clone();
    let chain_state = poster.chain.state.clone();
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
    assert!(matches!(
        rx.recv().await,
        Some(Request::CheckpointStatus { health: Some(Health::Pending | Health::Diverged), .. })
    ));
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(
        rx.recv().await,
        Some(Request::CheckpointStatus { health: Some(Health::Pending | Health::Diverged), .. })
    ));

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
    // Retries while diverged are one episode; the repaired vault lets the successor post, which ends it.
    tokio::time::advance(Duration::from_secs(2)).await;
    {
        let mut state = chain_state.lock().unwrap();
        state.distributions_paid = Fixed::from_int(40);
        state.balance = Fixed::from_int(60);
    }
    for _ in 0..8 {
        tokio::time::advance(Duration::from_millis(100)).await;
        if timeout(Duration::from_millis(50), rx.recv()).await.is_ok_and(|status| {
            matches!(status, Some(Request::CheckpointStatus { posted: Some(6), health: Some(Health::Healthy), .. }))
        }) {
            break;
        }
    }
    assert_eq!(*posts.lock().unwrap(), 2);
    drop(work_tx);
    task.await.unwrap().unwrap();
    let lines = captured.queued();
    assert!(lines.iter().any(|line| line.name == "collateral_mismatch"));
    let key = "checkpoint_withheld{cause=post_diverged}".to_owned();
    let withheld = lines.into_iter().filter(|line| line.name == "checkpoint_withheld").collect::<Vec<_>>();
    assert!(withheld.len() > 1, "every retry while diverged logs the hold");
    assert_eq!(
        crate::alerts::pages_for(withheld),
        [(crate::alerts::Status::Firing, key.clone()), (crate::alerts::Status::Resolved, key)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_repaired_vault_clears_the_withholding_on_the_next_retry() {
    let (_guard, _alerts) = crate::test_support::capture_alerts();
    let (poster, mut work, key) = poster();
    let posts = poster.chain.posts.clone();
    let chain_state = poster.chain.state.clone();
    // The vault overpaid its distributions: the first checkpoint posts, the verdict is Diverged,
    // and a signed successor waits behind it.
    work.distributed = engine_core::UsdcUnits(40_000_000);
    {
        let mut state = poster.chain.state.lock().unwrap();
        state.distributions_paid = Fixed::from_int(50);
        state.balance = work.liabilities - Fixed::from_int(50);
    }
    let parent = crate::log::CheckpointParent { has_checkpoint: true, seq: 5, root: [3; 32].into() };
    let domain = crate::log::CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
    let successor = Work {
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
    let (work_tx, work_rx) = mpsc::channel(4);
    let (tx, mut rx) = mpsc::channel(8);
    work_tx.send(work).await.unwrap();
    work_tx.send(successor).await.unwrap();
    let task = tokio::spawn(poster.run(work_rx, tx));
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { health: Some(Health::Diverged), .. })));
    // The operator repairs the vault: paid distributions no longer exceed the allocation, and the
    // compensated balance returns to the ledger. Withholding is read-only, so the next retry of
    // the successor reconciles and posts it.
    {
        let mut state = chain_state.lock().unwrap();
        state.distributions_paid = Fixed::from_int(40);
        state.balance = Fixed::from_int(60);
    }
    let mut saw_healthy = false;
    for _ in 0..8 {
        tokio::time::advance(Duration::from_millis(100)).await;
        if let Ok(Some(Request::CheckpointStatus { posted: Some(6), health: Some(Health::Healthy), .. })) =
            timeout(Duration::from_millis(50), rx.recv()).await
        {
            saw_healthy = true;
            break;
        }
    }
    assert!(saw_healthy, "the repaired vault reconciles and the successor posts");
    assert_eq!(*posts.lock().unwrap(), 2);
    drop(work_tx);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn allocated_distributions_capture_cumulative_credits_and_reconcile_before_and_after_claims() {
    use crate::checkpoints::DepositCoverage;
    use crate::checkpoints::accounting::Accounting;
    use engine_core::command::Sequenced;

    let (poster, template, _key) = poster();
    let mut engine = engine_core::Engine::new();
    let mut accounting = Accounting::default();
    let mut allocated = false;
    for line in include_str!("../../../../core/tests/reference/v25.jsonl").lines() {
        let command: Sequenced = serde_json::from_str(line).unwrap();
        let events = engine.apply(&command).unwrap();
        allocated |=
            events.iter().any(|event| matches!(event, engine_core::event::Event::DistributionAllocated { .. }));
        accounting.apply(&command.cmd, &events).unwrap();
    }
    assert!(allocated, "the fixture must execute an actual distribution allocation");
    let mut work = Work::capture(template.checkpoint.clone(), accounting, &engine);
    work.coverage = DepositCoverage::Canonical;
    assert!(work.distributed.0 > 0);
    let paid = Fixed::from_raw(i128::try_from(work.distributed.0).unwrap() * 1_000);
    {
        let mut state = poster.chain.state.lock().unwrap();
        state.deposits = work.deposits;
        state.deposited = work.liabilities;
        state.balance = work.liabilities;
    }
    *poster.chain.through.lock().unwrap() = vec![work.liabilities; work.deposits as usize];
    assert_eq!(poster.reconcile(&work).await.unwrap().1, Health::Healthy, "allocated, not yet claimed");
    {
        let mut state = poster.chain.state.lock().unwrap();
        state.distributions_paid = paid;
        state.balance -= paid;
    }
    assert_eq!(poster.reconcile(&work).await.unwrap().1, Health::Healthy, "claims explain the balance reduction");
}
