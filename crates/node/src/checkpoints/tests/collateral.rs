//! Collateral health: shortfalls, ledger differences, surpluses and uncredited deposits.

use std::time::Duration;

use engine_core::Fixed;
use tokio::sync::mpsc;

use super::poster;
use crate::checkpoints::Health;
use crate::sequencer::Request;

#[tokio::test]
async fn only_a_shortfall_or_a_ledger_difference_pauses_and_a_surplus_warns() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let (poster, mut work, _key) = poster();
    let mut next_alert = || {
        std::iter::from_fn(|| alerts.try_recv().ok())
            .find_map(|alert| Some((alert.get("alert")?.clone(), alert.get("level")?.clone())))
    };
    let alert = |name: &str, level: &str| Some((name.to_owned(), level.to_owned()));
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), Health::Healthy));
    // Deposits the checkpoint has not credited are compared through its last credited deposit.
    poster.chain.deposit(Fixed::from_int(10));
    poster.chain.deposit(Fixed::from_int(20));
    assert_eq!(
        poster.reconcile(&work).await.unwrap(),
        (Some(5), Health::Healthy),
        "in-flight deposits keep payouts running"
    );
    assert_eq!(next_alert(), None);
    // USDC sent outside a deposit keeps payouts running, with a Warning until it is booked.
    poster.chain.state.lock().unwrap().balance += Fixed::ONE;
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), Health::Healthy), "a surplus keeps payouts running");
    assert_eq!(next_alert(), alert("collateral_surplus", "WARN"));
    poster.chain.state.lock().unwrap().balance -= Fixed::ONE;
    poster.chain.deposit(Fixed::ONE);
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), Health::Healthy));
    assert_eq!(next_alert(), None, "a booked surplus is a deposit");
    // A shortfall pauses.
    poster.chain.state.lock().unwrap().balance -= Fixed::ONE;
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), Health::Diverged));
    assert_eq!(next_alert(), alert("collateral_mismatch", "ERROR"));
    poster.chain.state.lock().unwrap().balance += Fixed::ONE;
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), Health::Healthy));
    // A ledger that credited another amount, or more deposits than the vault holds, pauses.
    for (deposits, liabilities) in [(1, Fixed::from_int(101)), (2, Fixed::from_int(100)), (5, Fixed::from_int(131))] {
        (work.deposits, work.liabilities) = (deposits, liabilities);
        assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), Health::Diverged), "{deposits} deposits");
        assert_eq!(next_alert(), alert("collateral_mismatch", "ERROR"));
    }
    (work.deposits, work.liabilities) = (3, Fixed::from_int(130));
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), Health::Healthy), "a later checkpoint credits them");
}

#[tokio::test]
async fn an_alert_resolves_once_when_its_episode_ends_and_a_clear_vault_resolves_nothing_more() {
    let (_guard, _alerts, mut resolved) = crate::test_support::capture_alerts_and_resolutions();
    let (poster, work, _key) = poster();
    let mut drained = std::collections::HashMap::<String, usize>::new();
    let mut resolutions = |name: &str| {
        for log in std::iter::from_fn(|| resolved.try_recv().ok()) {
            *drained.entry(log["alert_resolved"].to_owned()).or_default() += 1;
        }
        drained.remove(name).unwrap_or(0)
    };
    let names = ["collateral_surplus", "collateral_mismatch", "checkpoint_post_failed", "collateral_pending_deposits"];
    // Several polls of a clear vault: the first after a start ends what a previous run may have
    // left open, and no later one has an episode to end.
    poster.reconcile(&work).await.unwrap();
    for name in names {
        assert_eq!(resolutions(name), 1, "{name}: a start resolves what an earlier run left open");
    }
    for _ in 0..5 {
        poster.reconcile(&work).await.unwrap();
    }
    for name in names {
        assert_eq!(resolutions(name), 0, "{name}: no episode was open");
    }
    // A surplus resolves once, however many polls follow it.
    poster.chain.state.lock().unwrap().balance += Fixed::ONE;
    for _ in 0..3 {
        poster.reconcile(&work).await.unwrap();
    }
    assert_eq!(resolutions("collateral_surplus"), 0, "a standing surplus resolves nothing");
    poster.chain.state.lock().unwrap().balance -= Fixed::ONE;
    for _ in 0..5 {
        poster.reconcile(&work).await.unwrap();
    }
    assert_eq!(resolutions("collateral_surplus"), 1);
    // So does a shortfall.
    poster.chain.state.lock().unwrap().balance -= Fixed::ONE;
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), Health::Diverged));
    poster.chain.state.lock().unwrap().balance += Fixed::ONE;
    for _ in 0..5 {
        poster.reconcile(&work).await.unwrap();
    }
    assert_eq!(resolutions("collateral_mismatch"), 1);
}

#[tokio::test(start_paused = true)]
async fn the_first_deposit_left_uncredited_past_the_deadline_pauses_until_a_checkpoint_credits_it() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let (poster, mut work, _key) = poster();
    let mut uncredited = None;
    poster.chain.deposit(Fixed::from_int(10));
    assert_eq!(
        poster.reconcile_lifecycle(&work, &[&work], &[], &mut uncredited).await.unwrap().1,
        Health::Healthy,
        "in flight"
    );
    tokio::time::advance(Duration::from_millis(600)).await;
    // A later deposit does not restart the first one's deadline.
    poster.chain.deposit(Fixed::from_int(20));
    assert_eq!(poster.reconcile_lifecycle(&work, &[&work], &[], &mut uncredited).await.unwrap().1, Health::Healthy);
    tokio::time::advance(Duration::from_millis(600)).await;
    assert_eq!(
        poster.reconcile_lifecycle(&work, &[&work], &[], &mut uncredited).await.unwrap().1,
        Health::Pending,
        "deposit 1 is overdue"
    );
    assert!(
        std::iter::from_fn(|| alerts.try_recv().ok()).any(|alert| alert["alert"] == "collateral_mismatch"
            && alert["message"].contains("deposit 1 is still uncredited"))
    );
    // A checkpoint that credits it moves the deadline to the next uncredited deposit.
    (work.deposits, work.liabilities) = (2, Fixed::from_int(110));
    assert_eq!(poster.reconcile_lifecycle(&work, &[&work], &[], &mut uncredited).await.unwrap().1, Health::Healthy);
    tokio::time::advance(Duration::from_millis(1_200)).await;
    assert_eq!(
        poster.reconcile_lifecycle(&work, &[&work], &[], &mut uncredited).await.unwrap().1,
        Health::Pending,
        "deposit 2 is overdue"
    );
    (work.deposits, work.liabilities) = (3, Fixed::from_int(130));
    assert_eq!(poster.reconcile_lifecycle(&work, &[&work], &[], &mut uncredited).await.unwrap().1, Health::Healthy);
    assert_eq!(uncredited, None);
}

#[tokio::test(start_paused = true)]
async fn the_worker_carries_an_uncredited_deposit_across_retries_to_its_deadline() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let (poster, work, _key) = poster();
    poster.chain.deposit(Fixed::from_int(10));
    let (work_tx, work_rx) = mpsc::channel(4);
    let (tx, mut rx) = mpsc::channel(4);
    work_tx.send(work).await.unwrap();
    let task = tokio::spawn(poster.run(work_rx, tx));
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { health: Some(Health::Healthy), .. })));
    tokio::time::advance(Duration::from_secs(2)).await;
    let mut paused = false;
    for _ in 0..8 {
        if matches!(rx.recv().await, Some(Request::CheckpointStatus { health: Some(Health::Pending), .. })) {
            paused = true;
            break;
        }
    }
    assert!(paused, "the deadline spans retries");
    assert!(std::iter::from_fn(|| alerts.try_recv().ok()).any(|alert| alert["alert"] == "collateral_mismatch"));
    drop(work_tx);
    while rx.recv().await.is_some() {}
    task.await.unwrap().unwrap();
}
