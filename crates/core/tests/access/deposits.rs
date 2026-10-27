use crate::common::f;
use crate::support::*;
use engine_core::Engine;
use engine_core::access::{AccessEvidence, AccessReject, SanctionsStatus, SourceFailure};
use engine_core::command::{Command, Sequenced};
use engine_core::event::{Event, Reject};

#[test]
fn sanctioned_payers_cannot_fund_innocent_beneficiaries_or_the_house_and_holds_never_auto_release() {
    for house in [false, true] {
        let (mut h, client) = setup();
        attest(&mut h, &client);
        let account = (!house).then_some(client.account);
        let (deposit, mut evidence) = deposit(&h, account, Some(client.account), "0xsanctioned:1");
        let AccessEvidence::Screened(screening) = &mut evidence else { unreachable!() };
        screening.payer_status = SanctionsStatus::Sanctioned;
        sign_screening(&h, screening);
        let cash = h.engine.house_cash();
        let events = h.cmd(Command::AccessDeposit { deposit: deposit.clone(), evidence });
        assert_eq!(
            events,
            [Event::DepositHeld {
                chain_ref: deposit.chain_ref.clone(),
                account,
                payer: deposit.payer,
                amount: deposit.amount,
                reason: AccessReject::PayerSanctioned
            }]
        );
        assert!(!h.engine.is_credited(&deposit.chain_ref));
        assert!(!h.engine.accounts().contains_key(&client.account));
        assert_eq!(h.engine.house_cash(), cash);
        let clear =
            crate::support::evidence(&h, engine_core::eip712::deposit_screening_hash(&deposit), account, deposit.payer);
        assert!(
            h.cmd(Command::AccessDeposit { deposit: deposit.clone(), evidence: clear })
                .iter()
                .any(|event| matches!(event, Event::DepositHeld { reason: AccessReject::DepositHeld, .. }))
        );
        assert_eq!(h.engine.held_deposits()[&deposit.chain_ref].reason, AccessReject::PayerSanctioned);
        assert_eq!(h.engine.house_cash(), cash);
        assert!(!h.engine.accounts().contains_key(&client.account));
    }
}

#[test]
fn only_a_confirmed_sanction_or_a_missing_payer_holds_a_deposit() {
    for expected in [AccessReject::AccountSanctioned, AccessReject::PayerSanctioned, AccessReject::PayerMissing] {
        let (mut h, client) = setup();
        attest(&mut h, &client);
        let payer = (expected != AccessReject::PayerMissing).then_some(client.account);
        let (deposit, mut evidence) = deposit(&h, Some(client.account), payer, "0xheld:1");
        // A confirmed sanction of either subject holds even when the other is unclassified.
        let AccessEvidence::Screened(screening) = &mut evidence else { unreachable!() };
        match expected {
            AccessReject::AccountSanctioned => {
                (screening.account_status, screening.payer_status) =
                    (SanctionsStatus::Sanctioned, SanctionsStatus::Unknown)
            }
            AccessReject::PayerSanctioned => {
                (screening.account_status, screening.payer_status) =
                    (SanctionsStatus::Unknown, SanctionsStatus::Sanctioned)
            }
            _ => {}
        }
        sign_screening(&h, screening);
        assert_eq!(
            h.cmd(Command::AccessDeposit { deposit: deposit.clone(), evidence }),
            [Event::DepositHeld {
                chain_ref: deposit.chain_ref.clone(),
                account: deposit.account,
                payer,
                amount: deposit.amount,
                reason: expected
            }]
        );
        assert!(!h.engine.is_credited(&deposit.chain_ref));
        assert!(h.engine.pending_deposits().is_empty());
        assert_eq!(h.engine.held_deposits()[&deposit.chain_ref].reason, expected);
        assert!(!h.engine.accounts().contains_key(&client.account));
    }
}

#[test]
fn clear_funding_credits_once_and_raw_deposits_cannot_bypass_access() {
    let (mut h, client) = setup();
    attest(&mut h, &client);
    fund(&mut h, &client);
    let (deposit, evidence) = deposit(&h, Some(client.account), Some(client.account), "0xfunded:1");
    assert_eq!(
        h.cmd(Command::AccessDeposit { deposit: deposit.clone(), evidence }),
        [Event::DepositRejected {
            account: client.account,
            amount: deposit.amount,
            chain_ref: deposit.chain_ref,
            reason: Reject::DuplicateReference
        }]
    );
    assert_eq!(h.engine.accounts()[&client.account].balance, f("100000"));
    // Raw ingress names no payer, so it can never be screened.
    let events = h.cmd(Command::Deposit { account: client.account, amount: f("10"), chain_ref: "0xraw:1".into() });
    assert!(matches!(events.as_slice(), [Event::DepositHeld { reason: AccessReject::PayerMissing, .. }]));
    assert!(!h.engine.is_credited("0xraw:1") && h.engine.held_deposits().contains_key("0xraw:1"));
    assert_eq!(h.engine.accounts()[&client.account].balance, f("100000"));
    let cash = h.engine.house_cash();
    let events = h.cmd(Command::HouseCapital { amount: f("-1"), chain_ref: "0xnegative:1".into() });
    assert!(events.iter().any(|event| matches!(event, Event::AdminRejected { .. })));
    assert_eq!(h.engine.house_cash(), cash);
}

#[test]
fn the_genesis_capital_exception_cannot_credit_a_previously_held_reference() {
    let (mut h, client) = setup_with_uncredited_genesis();
    assert!(!h.engine.is_credited("genesis"));
    let cash = h.engine.house_cash();
    let (deposit, mut evidence) = deposit(&h, None, Some(client.account), "genesis");
    let AccessEvidence::Screened(screening) = &mut evidence else { unreachable!() };
    screening.payer_status = SanctionsStatus::Sanctioned;
    sign_screening(&h, screening);
    let held = h.cmd(Command::AccessDeposit { deposit: deposit.clone(), evidence });
    assert!(matches!(held.as_slice(), [Event::DepositHeld { reason: AccessReject::PayerSanctioned, .. }]));
    let original = h.engine.held_deposits()["genesis"].clone();
    let raw = h.cmd(Command::HouseCapital { amount: deposit.amount, chain_ref: "genesis".into() });
    assert!(matches!(raw.as_slice(), [Event::DepositHeld { reason: AccessReject::DepositHeld, .. }]));
    let clear =
        crate::support::evidence(&h, engine_core::eip712::deposit_screening_hash(&deposit), None, deposit.payer);
    let redelivered = h.cmd(Command::AccessDeposit { deposit, evidence: clear });
    assert!(matches!(redelivered.as_slice(), [Event::DepositHeld { reason: AccessReject::DepositHeld, .. }]));
    assert!(!h.engine.is_credited("genesis"));
    assert_eq!(h.engine.held_deposits()["genesis"], original);
    assert_eq!(h.engine.house_cash(), cash);
}

#[test]
fn declarations_and_holds_replay_to_the_same_incremental_and_rebuilt_state_roots() {
    let (mut h, client) = setup();
    attest(&mut h, &client);
    let (pending, _) = deposit(&h, Some(client.account), Some(client.account), "0xheld:1");
    h.cmd(Command::AccessDeposit {
        deposit: pending,
        evidence: AccessEvidence::Failed { reason: SourceFailure::Unavailable },
    });
    let (deposit, mut evidence) = deposit(&h, Some(client.account), Some(client.account), "0xpermanent-hold:1");
    let AccessEvidence::Screened(screening) = &mut evidence else { unreachable!() };
    screening.payer_status = SanctionsStatus::Sanctioned;
    sign_screening(&h, screening);
    h.cmd(Command::AccessDeposit { deposit, evidence });
    assert_eq!(h.engine.pending_deposits().len(), 1);
    assert_eq!(h.engine.held_deposits().len(), 1);
    let expected = h.engine.state_root();
    let encoded = serde_json::to_vec(&h.engine).unwrap();
    let mut rebuilt: Engine = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(rebuilt.state_root(), expected);
    let mut replay = Engine::new();
    for Sequenced { seq, ts, cmd } in &h.log {
        replay.apply(&Sequenced { seq: *seq, ts: *ts, cmd: cmd.clone() }).unwrap();
        replay.state_root();
    }
    assert_eq!(replay.state_root(), expected);
    assert_eq!(replay.eligibility(client.account), h.engine.eligibility(client.account));
    assert_eq!(replay.held_deposits(), h.engine.held_deposits());
    assert_eq!(replay.pending_deposits(), h.engine.pending_deposits());
}
