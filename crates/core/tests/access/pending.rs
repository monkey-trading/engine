use crate::support::*;
use engine_core::Engine;
use engine_core::access::{
    AccessEvidence, AccessReject, EligibilityClass, Jurisdiction, SanctionsStatus, SourceFailure,
};
use engine_core::command::Command;
use engine_core::eip712::Signature;
use engine_core::event::{Event, Reject};

#[test]
fn unavailable_screening_survives_restart_and_credits_once_only_after_an_explicit_retry() {
    for house in [false, true] {
        let (mut h, client) = setup();
        attest(&mut h, &client);
        let account = (!house).then_some(client.account);
        let cash = h.engine.house_cash();
        let (deposit, clear) = deposit(&h, account, Some(client.account), "0xpending:1");
        let unavailable = AccessEvidence::Failed { reason: SourceFailure::Unavailable };
        let events = h.cmd(Command::AccessDeposit { deposit: deposit.clone(), evidence: unavailable.clone() });
        assert!(matches!(events.as_slice(), [Event::DepositPending { reason: AccessReject::SourceUnavailable, .. }]));
        assert!(!h.engine.is_credited(&deposit.chain_ref));
        assert!(!h.engine.accounts().contains_key(&client.account));
        assert_eq!(h.engine.house_cash(), cash);
        assert!(h.engine.held_deposits().is_empty());
        let duplicate = h.cmd(Command::AccessDeposit { deposit: deposit.clone(), evidence: clear.clone() });
        assert!(matches!(duplicate.as_slice(), [Event::DepositPending { reason: AccessReject::DepositPending, .. }]));
        let root = h.engine.state_root();
        let mut restored: Engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
        assert_eq!(restored.state_root(), root);
        assert_eq!(restored.pending_deposits()[&deposit.chain_ref], deposit);
        h.engine = restored;
        let retry =
            h.cmd(Command::RetryDepositScreening { chain_ref: deposit.chain_ref.clone(), evidence: unavailable });
        assert!(matches!(retry.as_slice(), [Event::DepositPending { reason: AccessReject::SourceUnavailable, .. }]));
        assert_root_matches_rebuild(&mut h.engine);
        let events =
            h.cmd(Command::RetryDepositScreening { chain_ref: deposit.chain_ref.clone(), evidence: clear.clone() });
        assert!(events.iter().any(|event| matches!(event, Event::Deposited { .. } | Event::HouseCapitalMoved { .. })));
        assert!(h.engine.is_credited(&deposit.chain_ref));
        assert!(h.engine.pending_deposits().is_empty() && h.engine.held_deposits().is_empty());
        assert_root_matches_rebuild(&mut h.engine);
        let repeated = h.cmd(Command::RetryDepositScreening { chain_ref: deposit.chain_ref, evidence: clear });
        assert!(matches!(repeated.as_slice(), [Event::AdminRejected { .. }]));
        if house {
            assert_eq!(h.engine.house_cash(), cash + deposit.amount);
        } else {
            assert_eq!(h.engine.accounts()[&client.account].balance, deposit.amount);
        }
    }
}

#[test]
fn a_pending_deposit_confirmed_sanctioned_becomes_a_hold_that_retry_cannot_release() {
    let (mut h, client) = setup();
    attest(&mut h, &client);
    let (deposit, mut sanctioned) = deposit(&h, Some(client.account), Some(client.account), "0xpending-sanction:1");
    h.cmd(Command::AccessDeposit {
        deposit: deposit.clone(),
        evidence: AccessEvidence::Failed { reason: SourceFailure::Unavailable },
    });
    assert_root_matches_rebuild(&mut h.engine);
    let AccessEvidence::Screened(screening) = &mut sanctioned else { unreachable!() };
    screening.payer_status = SanctionsStatus::Sanctioned;
    sign_screening(&h, screening);
    let events = h.cmd(Command::RetryDepositScreening { chain_ref: deposit.chain_ref.clone(), evidence: sanctioned });
    assert!(matches!(events.as_slice(), [Event::DepositHeld { reason: AccessReject::PayerSanctioned, .. }]));
    assert_root_matches_rebuild(&mut h.engine);
    let clear = evidence(&h, engine_core::eip712::deposit_screening_hash(&deposit), deposit.account, deposit.payer);
    let retry = h.cmd(Command::RetryDepositScreening { chain_ref: deposit.chain_ref.clone(), evidence: clear });
    assert!(matches!(retry.as_slice(), [Event::AdminRejected { .. }]));
    let repeated = h.cmd(Command::AccessDeposit {
        deposit: deposit.clone(),
        evidence: AccessEvidence::Failed { reason: SourceFailure::Unavailable },
    });
    assert!(matches!(repeated.as_slice(), [Event::DepositHeld { reason: AccessReject::DepositHeld, .. }]));
    assert!(h.engine.pending_deposits().is_empty());
    assert_eq!(h.engine.held_deposits()[&deposit.chain_ref].reason, AccessReject::PayerSanctioned);
    assert!(!h.engine.is_credited(&deposit.chain_ref));
    assert!(!h.engine.accounts().contains_key(&client.account));
}

#[test]
fn every_deposit_source_failure_stays_pending_until_a_verified_retry_credits_it_once() {
    let cases = [
        (AccessReject::SourceUnavailable, Some(SourceFailure::Unavailable)),
        (AccessReject::SourceInvalid, Some(SourceFailure::InvalidResponse)),
        (AccessReject::SourceNotConfigured, Some(SourceFailure::NotConfigured)),
        (AccessReject::PeerUnknown, Some(SourceFailure::MissingPeer)),
        (AccessReject::SourceInvalid, None),
        (AccessReject::EvidenceStale, None),
        (AccessReject::EvidenceMismatch, None),
    ];
    for (expected, failure) in cases {
        let (mut h, client) = setup();
        attest(&mut h, &client);
        let (deposit, clear) = deposit(&h, Some(client.account), Some(client.account), "0xsource-failure:1");
        let mut evidence = clear.clone();
        let AccessEvidence::Screened(screening) = &mut evidence else { unreachable!() };
        match expected {
            AccessReject::EvidenceStale => screening.observed_at -= 1_001,
            AccessReject::EvidenceMismatch => screening.payer = None,
            _ => {}
        }
        sign_screening(&h, screening);
        if expected == AccessReject::SourceInvalid {
            screening.signature = Signature::ZERO;
        }
        let evidence = failure.map_or(evidence, |reason| AccessEvidence::Failed { reason });
        let events = h.cmd(Command::AccessDeposit { deposit: deposit.clone(), evidence });
        assert!(
            matches!(events.as_slice(), [Event::DepositPending { reason, .. }] if *reason == expected),
            "{expected:?}: {events:?}"
        );
        assert!(h.engine.held_deposits().is_empty(), "{expected:?} must not hold the deposit for good");
        assert!(!h.engine.is_credited(&deposit.chain_ref) && !h.engine.accounts().contains_key(&client.account));
        let retried = h.cmd(Command::RetryDepositScreening { chain_ref: deposit.chain_ref.clone(), evidence: clear });
        assert!(retried.contains(&Event::Deposited { account: client.account, amount: deposit.amount }), "{retried:?}");
        assert!(h.engine.pending_deposits().is_empty() && h.engine.held_deposits().is_empty());
        assert_root_matches_rebuild(&mut h.engine);
    }
}

#[test]
fn undeclared_refused_or_unclassified_deposits_stay_pending_and_credit_once_cured() {
    for expected in [AccessReject::EligibilityRequired, AccessReject::AccountUnknown, AccessReject::PayerUnknown] {
        let (mut h, client) = setup();
        if expected != AccessReject::EligibilityRequired {
            attest(&mut h, &client);
        }
        let (deposit, clear) = deposit(&h, Some(client.account), Some(client.account), "0xcurable:1");
        let mut evidence = clear.clone();
        let AccessEvidence::Screened(screening) = &mut evidence else { unreachable!() };
        match expected {
            AccessReject::AccountUnknown => screening.account_status = SanctionsStatus::Unknown,
            AccessReject::PayerUnknown => screening.payer_status = SanctionsStatus::Unknown,
            _ => {}
        }
        sign_screening(&h, screening);
        let events = h.cmd(Command::AccessDeposit { deposit: deposit.clone(), evidence });
        assert!(
            matches!(events.as_slice(), [Event::DepositPending { reason, .. }] if *reason == expected),
            "{expected:?}: {events:?}"
        );
        assert!(h.engine.held_deposits().is_empty(), "{expected:?} is curable and must not be held");
        assert!(!h.engine.is_credited(&deposit.chain_ref) && !h.engine.accounts().contains_key(&client.account));
        if expected == AccessReject::EligibilityRequired {
            let retry =
                Command::RetryDepositScreening { chain_ref: deposit.chain_ref.clone(), evidence: clear.clone() };
            let early = h.cmd(retry);
            assert!(matches!(
                early.as_slice(),
                [Event::DepositPending { reason: AccessReject::EligibilityRequired, .. }]
            ));
            // A declaration from a restricted jurisdiction is refused, so the deposit waits on.
            let refused = declaration(&h, &client, EligibilityClass::Professional);
            let mut evidence = declaration_evidence(&h, &refused);
            let AccessEvidence::Screened(screening) = &mut evidence else { unreachable!() };
            screening.jurisdiction = Jurisdiction::UnitedStates;
            sign_screening(&h, screening);
            let events = h.cmd(Command::AttestEligibility { request: refused, evidence });
            let reason = Reject::Access { reason: AccessReject::UnitedStates };
            assert_eq!(events, [Event::EligibilityRejected { account: client.account, reason }]);
            assert!(h.engine.retryable_pending("", 8).is_empty(), "a refused declaration makes nothing retryable");
            let retry =
                Command::RetryDepositScreening { chain_ref: deposit.chain_ref.clone(), evidence: clear.clone() };
            assert!(matches!(
                h.cmd(retry).as_slice(),
                [Event::DepositPending { reason: AccessReject::EligibilityRequired, .. }]
            ));
            attest(&mut h, &client);
        }
        let retried = h.cmd(Command::RetryDepositScreening { chain_ref: deposit.chain_ref.clone(), evidence: clear });
        assert!(retried.contains(&Event::Deposited { account: client.account, amount: deposit.amount }), "{retried:?}");
        assert!(h.engine.pending_deposits().is_empty() && h.engine.held_deposits().is_empty());
        assert_root_matches_rebuild(&mut h.engine);
    }
}

#[test]
fn indexed_retry_candidates_and_pending_lookups_match_a_full_scan_through_declarations() {
    let (mut h, client) = setup();
    let (other, stranger) = (crate::common::Client::new(2), crate::common::Client::new(3));
    let unavailable = AccessEvidence::Failed { reason: SourceFailure::Unavailable };
    // An undeclared account's, a declared account's and the house's deposits all wait pending.
    let (undeclared, clear) = deposit(&h, Some(client.account), Some(client.account), "0xindex:1");
    h.cmd(Command::AccessDeposit { deposit: undeclared.clone(), evidence: clear.clone() });
    attest(&mut h, &other);
    let (declared, _) = deposit(&h, Some(other.account), Some(other.account), "0xindex:2");
    h.cmd(Command::AccessDeposit { deposit: declared, evidence: unavailable.clone() });
    let (house, _) = deposit(&h, None, Some(other.account), "0xindex:3");
    h.cmd(Command::AccessDeposit { deposit: house, evidence: unavailable });
    let check = |engine: &Engine, retryable: &[&str], pending: [bool; 3]| {
        let scanned: Engine = serde_json::from_slice(&serde_json::to_vec(engine).unwrap()).unwrap();
        for engine in [engine, &scanned] {
            let references = |after, limit| -> Vec<String> {
                engine.retryable_pending(after, limit).into_iter().map(|(reference, _)| reference).collect()
            };
            assert_eq!(references("", 8), retryable);
            assert_eq!(references(retryable.last().unwrap(), 1), &retryable[..1], "the scan wraps around");
            let lookups = [client.account, other.account, stranger.account].map(|a| engine.has_pending_deposit(a));
            assert_eq!(lookups, pending);
        }
    };
    check(&h.engine, &["0xindex:2", "0xindex:3"], [true, true, false]);
    attest(&mut h, &client);
    check(&h.engine, &["0xindex:1", "0xindex:2", "0xindex:3"], [true, true, false]);
    let retry = h.cmd(Command::RetryDepositScreening { chain_ref: undeclared.chain_ref, evidence: clear });
    assert!(matches!(retry.as_slice(), [Event::Deposited { .. }]), "{retry:?}");
    check(&h.engine, &["0xindex:2", "0xindex:3"], [false, true, false]);
}

/// Deleted pending leaves must disappear from both the incremental tree and a fresh rebuild.
fn assert_root_matches_rebuild(engine: &mut Engine) {
    let incremental = engine.state_root();
    let mut rebuilt: Engine = serde_json::from_slice(&serde_json::to_vec(engine).unwrap()).unwrap();
    assert_eq!(rebuilt.state_root(), incremental);
}

#[test]
fn the_genesis_capital_exception_cannot_release_a_pending_reference() {
    let (mut h, client) = setup_with_uncredited_genesis();
    assert!(!h.engine.is_credited("genesis"));
    let cash = h.engine.house_cash();
    let (deposit, clear) = deposit(&h, None, Some(client.account), "genesis");
    h.cmd(Command::AccessDeposit {
        deposit: deposit.clone(),
        evidence: AccessEvidence::Failed { reason: SourceFailure::Unavailable },
    });
    let raw = h.cmd(Command::HouseCapital { amount: deposit.amount, chain_ref: "genesis".into() });
    assert!(matches!(raw.as_slice(), [Event::DepositPending { reason: AccessReject::DepositPending, .. }]));
    assert_eq!(h.engine.house_cash(), cash);
    assert_eq!(h.engine.pending_deposits()["genesis"], deposit);
    assert!(!h.engine.is_credited("genesis"));
    let retry = h.cmd(Command::RetryDepositScreening { chain_ref: "genesis".into(), evidence: clear });
    assert!(matches!(retry.as_slice(), [Event::HouseCapitalMoved { amount, .. }] if *amount == deposit.amount));
    assert!(h.engine.pending_deposits().is_empty());
    assert_eq!(h.engine.house_cash(), cash + deposit.amount);
}
