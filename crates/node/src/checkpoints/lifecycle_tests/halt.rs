//! A vault whose guardian paused withdrawals or retired the signer: observed, alerted, never a
//! conflict, and finalized before it supplies release evidence.

use super::*;

use crate::checkpoints::Conflict;
use crate::log::CheckpointParent;
use crate::test_support::{Alert, capture_alerts};

const RETIRED: AccountId = AccountId([0; 20]);

/// A named change to a vault observation.
type Change = (&'static str, fn(&mut Observation));

#[tokio::test]
async fn a_retired_signer_halts_withdrawals_without_a_conflict_or_a_post() {
    let (_guard, mut alerts) = capture_alerts();
    let (poster, work) = fixture();
    for view in [&poster.chain.latest, &poster.chain.finalized] {
        let mut at = view.lock().unwrap();
        at.state.signer = RETIRED;
        at.lifecycle.as_mut().unwrap().signer = RETIRED;
    }
    // The chain refuses posts by panicking: an unposted successor must not reach it.
    let next = successor(&work);
    let (posted, health, lifecycle) = poster.reconcile_lifecycle(&next, &[&work, &next], &[], &mut None).await.unwrap();
    assert_eq!((posted, health), (Some(5), Health::Healthy));
    let evidence = lifecycle.expect("a retired signer still yields finalized evidence");
    assert!(evidence.signer_retired() && evidence.halts_withdrawals());
    assert_eq!(halts(&mut alerts), ["signer_retired"]);
}

#[tokio::test]
async fn a_pause_halts_withdrawals_with_its_own_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let (poster, work) = fixture();
    for view in [&poster.chain.latest, &poster.chain.finalized] {
        view.lock().unwrap().lifecycle.as_mut().unwrap().withdrawals_paused = true;
    }
    let (posted, health, lifecycle) = poster.reconcile_lifecycle(&work, &[&work], &[], &mut None).await.unwrap();
    assert_eq!((posted, health), (Some(5), Health::Healthy));
    let evidence = lifecycle.unwrap();
    assert!(evidence.halts_withdrawals() && !evidence.signer_retired());
    assert_eq!(halts(&mut alerts), ["paused"]);

    poster.chain.finalized.lock().unwrap().lifecycle.as_mut().unwrap().withdrawals_paused = false;
    poster.chain.latest.lock().unwrap().lifecycle.as_mut().unwrap().withdrawals_paused = false;
    let (_, _, lifecycle) = poster.reconcile_lifecycle(&work, &[&work], &[], &mut None).await.unwrap();
    assert!(!lifecycle.unwrap().halts_withdrawals());
    assert!(halts(&mut alerts).is_empty(), "a cleared pause raises nothing");
}

#[tokio::test]
async fn a_halt_or_its_end_seen_only_in_the_latest_block_supplies_no_evidence() {
    let changes: [Change; 2] = [
        ("pause", |at| at.lifecycle.as_mut().unwrap().withdrawals_paused = true),
        ("retirement", |at| {
            at.state.signer = RETIRED;
            at.lifecycle.as_mut().unwrap().signer = RETIRED;
        }),
    ];
    for (name, change) in changes {
        for latest_changed in [true, false] {
            let (poster, work) = fixture();
            let view = if latest_changed { &poster.chain.latest } else { &poster.chain.finalized };
            change(&mut view.lock().unwrap());
            let error = poster.reconcile_lifecycle(&work, &[&work], &[], &mut None).await.unwrap_err();
            assert!(!error.is::<Conflict>(), "{name}: a transition is not a conflict: {error:#}");
            assert!(error.to_string().contains("not finalized"), "{name}, latest_changed={latest_changed}: {error:#}");
        }
    }
}

#[tokio::test]
async fn a_deposit_a_retired_signer_cannot_checkpoint_raises_no_collateral_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let (mut poster, work) = fixture();
    poster.pending_deposits_critical_after = Duration::ZERO;
    poster.chain.deposited_through.lock().unwrap().insert(1, Fixed::from_int(100));
    for view in [&poster.chain.latest, &poster.chain.finalized] {
        let mut at = view.lock().unwrap();
        at.state.signer = RETIRED;
        at.lifecycle.as_mut().unwrap().signer = RETIRED;
        at.state.deposits = 2;
        at.state.deposited = Fixed::from_int(110);
        at.state.balance = Fixed::from_int(110);
    }
    let mut uncredited = None;
    for _ in 0..2 {
        let (_, health, _) = poster.reconcile_lifecycle(&work, &[&work], &[], &mut uncredited).await.unwrap();
        assert_eq!(health, Health::Healthy, "the halt, not the catch-up deadline, holds withdrawals");
    }
    assert!(uncredited.is_none());
    assert!(!drained(&mut alerts).contains(&"collateral_mismatch".to_owned()));

    // With a signer, the same deposit past its deadline pages.
    for view in [&poster.chain.latest, &poster.chain.finalized] {
        let mut at = view.lock().unwrap();
        at.state.signer = poster.signer;
        at.lifecycle.as_mut().unwrap().signer = poster.signer;
    }
    let (_, health, _) = poster.reconcile_lifecycle(&work, &[&work], &[], &mut uncredited).await.unwrap();
    assert_eq!(health, Health::Pending);
    assert!(drained(&mut alerts).contains(&"collateral_mismatch".to_owned()));
}

#[tokio::test]
async fn a_foreign_signer_still_conflicts() {
    let (poster, work) = fixture();
    for view in [&poster.chain.latest, &poster.chain.finalized] {
        let mut at = view.lock().unwrap();
        at.state.signer = AccountId([9; 20]);
        at.lifecycle.as_mut().unwrap().signer = AccountId([9; 20]);
    }
    let error = poster.reconcile_lifecycle(&work, &[&work], &[], &mut None).await.unwrap_err();
    assert!(error.is::<Conflict>(), "{error:#}");
}

/// The fixture's unposted child, signed by the same enclave.
fn successor(work: &Work) -> Work {
    let key = EnclaveKey::test_key();
    let domain = work.checkpoint.domain.unwrap();
    let parent = CheckpointParent { has_checkpoint: true, seq: 5, root: work.checkpoint.root.parse().unwrap() };
    let root = [4; 32];
    let checkpoint = Checkpoint {
        seq: 6,
        root: format!("0x{}", hex::encode(root)),
        signer: key.address_hex(),
        signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, 6, &root, parent),
        domain: Some(domain),
        parent: Some(parent),
        deferred_withdrawals: true,
    };
    Work { checkpoint, ..work.clone() }
}

/// The names of the alerts raised so far.
fn drained(alerts: &mut tokio::sync::mpsc::UnboundedReceiver<Alert>) -> Vec<String> {
    std::iter::from_fn(|| alerts.try_recv().ok()).map(|alert| alert["alert"].clone()).collect()
}

/// The causes of the Critical `vault_withdrawals_halted` alerts raised so far.
fn halts(alerts: &mut tokio::sync::mpsc::UnboundedReceiver<Alert>) -> Vec<String> {
    let mut causes = Vec::new();
    while let Ok(alert) = alerts.try_recv() {
        if alert["alert"] == "vault_withdrawals_halted" {
            assert_eq!(alert["level"], "ERROR", "a halt is Critical");
            causes.push(alert["cause"].clone());
        }
    }
    causes
}
