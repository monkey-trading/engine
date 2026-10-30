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

/// Which finalized halt the fixture's vault shows while its reconcile fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Halt {
    Paused,
    Retired,
}

impl Halt {
    /// Applies the halt to one view of the fixture's vault.
    fn apply(self, at: &mut Observation) {
        match self {
            Halt::Paused => at.lifecycle.as_mut().unwrap().withdrawals_paused = true,
            Halt::Retired => {
                at.state.signer = RETIRED;
                at.lifecycle.as_mut().unwrap().signer = RETIRED;
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_failed_reconcile_still_reports_a_finalized_halt() {
    for halt in [None, Some(Halt::Paused), Some(Halt::Retired)] {
        let (_guard, mut alerts) = capture_alerts();
        let (poster, work) = fixture();
        // The vault advanced to a checkpoint this worker has not received, so reconciling fails.
        {
            let mut latest = poster.chain.latest.lock().unwrap();
            latest.state.checkpoint = Some((7, "0x07".to_owned()));
            let evidence = latest.lifecycle.as_mut().unwrap();
            (evidence.checkpoint_seq, evidence.checkpoint_root) = (Some(7), Some("0x07".to_owned()));
        }
        if let Some(halt) = halt {
            for view in [&poster.chain.latest, &poster.chain.finalized] {
                halt.apply(&mut view.lock().unwrap());
            }
        }
        let (_, health, lifecycle) = status_parts(one_status(poster, work).await);
        let lifecycle = lifecycle.as_ref();
        assert!(health.is_none(), "halt={halt:?}: a failed reconcile reports no verdict");
        assert_eq!(lifecycle.is_some_and(|evidence| evidence.halts_withdrawals()), halt.is_some(), "halt={halt:?}");
        assert_eq!(
            lifecycle.is_some_and(|evidence| evidence.signer_retired()),
            halt == Some(Halt::Retired),
            "halt={halt:?}"
        );
        let causes = match halt {
            Some(Halt::Paused) => vec!["paused".to_owned()],
            Some(Halt::Retired) => vec!["signer_retired".to_owned()],
            None => Vec::new(),
        };
        assert_eq!(halts(&mut alerts), causes, "halt={halt:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_failed_reconcile_with_disagreeing_views_supplies_no_evidence() {
    let (_guard, mut alerts) = capture_alerts();
    let (poster, work) = fixture();
    {
        let mut latest = poster.chain.latest.lock().unwrap();
        latest.state.checkpoint = Some((7, "0x07".to_owned()));
        let evidence = latest.lifecycle.as_mut().unwrap();
        (evidence.checkpoint_seq, evidence.checkpoint_root) = (Some(7), Some("0x07".to_owned()));
        evidence.withdrawals_paused = true;
    }
    Halt::Retired.apply(&mut poster.chain.finalized.lock().unwrap());
    let (_, _, lifecycle) = status_parts(one_status(poster, work).await);
    assert!(lifecycle.is_none(), "latest and finalized must agree before a halt is reported");
    assert!(halts(&mut alerts).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_failed_reconcile_with_a_foreign_finalized_signer_supplies_no_evidence() {
    let (_guard, mut alerts) = capture_alerts();
    let (poster, work) = fixture();
    {
        let mut latest = poster.chain.latest.lock().unwrap();
        latest.state.checkpoint = Some((7, "0x07".to_owned()));
        let evidence = latest.lifecycle.as_mut().unwrap();
        (evidence.checkpoint_seq, evidence.checkpoint_root) = (Some(7), Some("0x07".to_owned()));
    }
    {
        let mut finalized = poster.chain.finalized.lock().unwrap();
        finalized.state.signer = AccountId([9; 20]);
        let evidence = finalized.lifecycle.as_mut().unwrap();
        evidence.signer = AccountId([9; 20]);
        evidence.withdrawals_paused = true;
    }
    let (_, _, lifecycle) = status_parts(one_status(poster, work).await);
    assert!(lifecycle.is_none(), "a halt under a foreign authority is not this enclave's evidence");
    assert!(halts(&mut alerts).is_empty());
}

#[tokio::test]
async fn a_cleared_pause_or_retirement_resolves_its_halt_alert() {
    for halt in [Halt::Paused, Halt::Retired] {
        let (poster, work) = fixture();
        let cause = match halt {
            Halt::Paused => "paused",
            Halt::Retired => "signer_retired",
        };
        let key = format!("vault_withdrawals_halted{{cause={cause}}}");
        let (guard, router) = crate::alerts::capture_until_dropped();
        for active in [true, false] {
            for view in [&poster.chain.latest, &poster.chain.finalized] {
                let mut at = view.lock().unwrap();
                match halt {
                    Halt::Paused => at.lifecycle.as_mut().unwrap().withdrawals_paused = active,
                    Halt::Retired if active => Halt::Retired.apply(&mut at),
                    Halt::Retired => {
                        at.state.signer = poster.signer;
                        at.lifecycle.as_mut().unwrap().signer = poster.signer;
                    }
                }
            }
            poster.reconcile_lifecycle(&work, &[&work], &[], &mut None).await.unwrap();
        }
        drop(guard);
        let pages = crate::alerts::pages_for(router.queued());
        assert_eq!(
            pages,
            vec![(crate::alerts::Status::Firing, key.clone()), (crate::alerts::Status::Resolved, key,)],
            "halt={halt:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_retirement_seen_before_the_first_finalized_checkpoint_reaches_the_sequencer() {
    let (_guard, mut alerts) = capture_alerts();
    let (poster, work) = fixture();
    // A fresh vault holds no finalized checkpoint when the guardian retires the signer.
    for view in [&poster.chain.latest, &poster.chain.finalized] {
        let mut at = view.lock().unwrap();
        at.state.signer = RETIRED;
        at.state.checkpoint = None;
        let lifecycle = at.lifecycle.as_mut().unwrap();
        lifecycle.signer = RETIRED;
        lifecycle.checkpoint_seq = None;
        lifecycle.checkpoint_root = None;
    }
    let (work_tx, work_rx) = mpsc::channel(4);
    let (status_tx, mut status_rx) = mpsc::channel(4);
    work_tx.send(work).await.unwrap();
    let task = tokio::spawn(poster.run_with_approvals(work_rx, status_tx, Vec::new(), registry(1)));
    let Request::CheckpointStatus { posted: None, health: None, lifecycle: Some(evidence) } =
        status_rx.recv().await.unwrap()
    else {
        panic!("the bootstrap retirement must reach the sequencer as finalized evidence");
    };
    assert!(evidence.signer_retired() && evidence.halts_withdrawals());
    assert!(evidence.checkpoint_seq.is_none(), "bootstrap evidence carries no checkpoint, so it releases nothing");
    assert_eq!(halts(&mut alerts), ["signer_retired"]);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_retirement_with_an_older_finalized_checkpoint_reaches_the_sequencer() {
    let (_guard, mut alerts) = capture_alerts();
    let (poster, work) = fixture();
    // Latest shows the posted checkpoint; the finalized view still holds only its predecessor.
    let older = format!("0x{}", "22".repeat(32));
    for view in [&poster.chain.latest, &poster.chain.finalized] {
        let mut at = view.lock().unwrap();
        at.state.signer = RETIRED;
        at.lifecycle.as_mut().unwrap().signer = RETIRED;
    }
    {
        let mut finalized = poster.chain.finalized.lock().unwrap();
        finalized.state.checkpoint = Some((4, older.clone()));
        let lifecycle = finalized.lifecycle.as_mut().unwrap();
        lifecycle.checkpoint_seq = Some(4);
        lifecycle.checkpoint_root = Some(older);
    }
    let (work_tx, work_rx) = mpsc::channel(4);
    let (status_tx, mut status_rx) = mpsc::channel(4);
    work_tx.send(work).await.unwrap();
    let task = tokio::spawn(poster.run_with_approvals(work_rx, status_tx, Vec::new(), registry(1)));
    let Request::CheckpointStatus { posted: None, health: None, lifecycle: Some(evidence) } =
        status_rx.recv().await.unwrap()
    else {
        panic!("the older-finalized retirement must reach the sequencer as finalized evidence");
    };
    assert!(evidence.signer_retired() && evidence.halts_withdrawals());
    assert_eq!(evidence.checkpoint_seq, Some(4));
    assert_eq!(halts(&mut alerts), ["signer_retired"]);
    task.abort();
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

/// Drives one tick of the fixture's worker and returns the single status it sends.
async fn one_status(poster: Poster<Chain>, work: Work) -> Request {
    let (work_tx, work_rx) = mpsc::channel(4);
    let (status_tx, mut status_rx) = mpsc::channel(4);
    work_tx.send(work).await.unwrap();
    let task = tokio::spawn(poster.run_with_approvals(work_rx, status_tx, Vec::new(), registry(1)));
    let status = status_rx.recv().await.unwrap();
    task.abort();
    status
}

/// The parts of a checkpoint status, for assertions.
fn status_parts(status: Request) -> (Option<u64>, Option<Health>, Option<LifecycleEvidence>) {
    match status {
        Request::CheckpointStatus { posted, health, lifecycle } => (posted, health, lifecycle),
        _ => panic!("expected a checkpoint status"),
    }
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
