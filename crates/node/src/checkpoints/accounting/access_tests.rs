use super::*;

use engine_core::Engine;
use engine_core::access::{AccessEvidence, SourceFailure};
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Sequenced};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::checkpoints::accounting::tests::{assert_matches, recompute};
use crate::test_support::dev_config;

#[test]
fn pending_reserves_move_to_cash_or_holds_without_recounting_vault_deposits() {
    use crate::test_support::access;
    use engine_core::access::SanctionsStatus;
    for (house, sanctioned) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut engine = Engine::new();
        let mut accounting = Accounting::default();
        let mut log = Vec::new();
        let mut apply = |engine: &mut Engine, accounting: &mut Accounting, cmd| {
            let command = Sequenced { seq: log.len() as u64, ts: 0, cmd };
            let events = engine.apply(&command).unwrap();
            accounting.apply(&command.cmd, &events).unwrap();
            assert_matches(*accounting, engine, Fixed::ZERO);
            log.push(command);
        };
        for cmd in [
            AdminCommand::SetClientAuth(ClientAuth::Unsigned),
            AdminCommand::SetOperator(dev_config().operator().unwrap()),
            AdminCommand::ActivateProtocol { version: ProtocolVersion::V24 },
            AdminCommand::SetAccessPolicy(access::policy()),
        ] {
            apply(&mut engine, &mut accounting, Command::Admin(cmd));
        }
        let declaration = access::declaration(&engine, 1);
        apply(&mut engine, &mut accounting, declaration);
        let account = (!house).then_some(access::account(1));
        let Command::AccessDeposit { deposit, evidence: clear } =
            access::funding(&engine, account, Some(access::account(1)), Fixed::from_int(50), "0xoutage:1")
        else {
            unreachable!()
        };
        let unavailable = AccessEvidence::Failed { reason: SourceFailure::Unavailable };
        for cmd in [
            Command::AccessDeposit { deposit: deposit.clone(), evidence: unavailable.clone() },
            Command::AccessDeposit { deposit: deposit.clone(), evidence: clear.clone() },
            Command::RetryDepositScreening { chain_ref: deposit.chain_ref.clone(), evidence: unavailable },
        ] {
            apply(&mut engine, &mut accounting, cmd);
        }
        assert_eq!(
            (accounting.pending, accounting.held, accounting.liabilities, accounting.deposits),
            (deposit.amount, Fixed::ZERO, deposit.amount, 1)
        );
        let mut decision = clear.clone();
        if sanctioned {
            let AccessEvidence::Screened(screening) = &mut decision else { unreachable!() };
            screening.payer_status = SanctionsStatus::Sanctioned;
            screening.signature = engine_core::eip712::sign(
                &access::key(0x77),
                &engine_core::eip712::screening_hash(&engine.access_domain().unwrap(), screening),
            );
        }
        for evidence in [decision, clear] {
            apply(
                &mut engine,
                &mut accounting,
                Command::RetryDepositScreening { chain_ref: deposit.chain_ref.clone(), evidence },
            );
        }
        assert_eq!((accounting.pending, accounting.liabilities, accounting.deposits), (Fixed::ZERO, deposit.amount, 1));
        assert_eq!(accounting.held, if sanctioned { deposit.amount } else { Fixed::ZERO });
        let mut replay = Engine::new();
        let mut recovered = Accounting::default();
        for command in log {
            let events = replay.apply(&command).unwrap();
            recovered.apply(&command.cmd, &events).unwrap();
            assert_matches(recovered, &replay, Fixed::ZERO);
        }
        assert_eq!(recovered, accounting);
    }
}

#[test]
fn held_collateral_is_reserved_once_and_captured_in_the_same_reconciliation_total_after_replay() {
    use crate::test_support::access;
    use engine_core::access::SanctionsStatus;
    let mut engine = Engine::new();
    let mut accounting = Accounting::default();
    let apply = |engine: &mut Engine, accounting: &mut Accounting, cmd: Command| {
        let sequenced = Sequenced { seq: engine.last_seq().map_or(0, |seq| seq + 1), ts: 0, cmd };
        let events = engine.apply(&sequenced).unwrap();
        accounting.apply(&sequenced.cmd, &events).unwrap();
        assert_matches(*accounting, engine, Fixed::ZERO);
        sequenced
    };
    let mut sequenced = Vec::new();
    for cmd in [
        AdminCommand::SetClientAuth(ClientAuth::Unsigned),
        AdminCommand::SetOperator(dev_config().operator().unwrap()),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::V24 },
        AdminCommand::SetAccessPolicy(access::policy()),
    ] {
        sequenced.push(apply(&mut engine, &mut accounting, Command::Admin(cmd)));
    }
    // A verified sanctioned payer holds both a trader's and the house's funding, each counted once.
    for (account, reference) in [(Some(AccountId([1; 20])), "0xtrader-hold:1"), (None, "0xhouse-hold:1")] {
        let Command::AccessDeposit { deposit, evidence: AccessEvidence::Screened(mut screening) } =
            access::funding(&engine, account, Some(AccountId([2; 20])), Fixed::from_int(50), reference)
        else {
            unreachable!()
        };
        screening.payer_status = SanctionsStatus::Sanctioned;
        screening.signature = engine_core::eip712::sign(
            &access::key(0x77),
            &engine_core::eip712::screening_hash(&engine.access_domain().unwrap(), &screening),
        );
        let evidence = AccessEvidence::Screened(screening);
        for _ in 0..2 {
            let cmd = Command::AccessDeposit { deposit: deposit.clone(), evidence: evidence.clone() };
            sequenced.push(apply(&mut engine, &mut accounting, cmd));
        }
    }
    assert_eq!(engine.held_deposits().len(), 2);
    assert_eq!(
        (accounting.liabilities, accounting.held, accounting.deposits),
        (Fixed::from_int(100), Fixed::from_int(100), 2)
    );
    assert!(engine.accounts().is_empty());
    assert_eq!(engine.house_nav(), Fixed::ZERO);
    assert_eq!(recompute(&engine, Fixed::ZERO), (Fixed::from_int(100), 2));
    let mut replay = Engine::new();
    let mut recovered = Accounting::default();
    for command in sequenced {
        let events = replay.apply(&command).unwrap();
        recovered.apply(&command.cmd, &events).unwrap();
    }
    assert_eq!(recovered, accounting);
    assert_matches(recovered, &replay, Fixed::ZERO);
}

#[test]
fn every_pending_refusal_reserves_once_and_settles_without_recounting_its_deposit() {
    use crate::test_support::access;
    use engine_core::access::AccessDeposit;
    // Two source failures, an undeclared beneficiary, and raw ingress, which names no payer and is held.
    for case in ["invalid response", "unconfigured", "undeclared", "raw"] {
        let mut engine = Engine::new();
        let mut accounting = Accounting::default();
        let mut log = Vec::new();
        let mut apply = |engine: &mut Engine, accounting: &mut Accounting, cmd| {
            let sequenced = Sequenced { seq: log.len() as u64, ts: 0, cmd };
            let events = engine.apply(&sequenced).unwrap();
            accounting.apply(&sequenced.cmd, &events).unwrap();
            assert_matches(*accounting, engine, Fixed::ZERO);
            log.push(sequenced);
        };
        for cmd in [
            AdminCommand::SetClientAuth(ClientAuth::Unsigned),
            AdminCommand::SetOperator(dev_config().operator().unwrap()),
            AdminCommand::ActivateProtocol { version: ProtocolVersion::V24 },
            AdminCommand::SetAccessPolicy(access::policy()),
        ] {
            apply(&mut engine, &mut accounting, Command::Admin(cmd));
        }
        if case != "undeclared" {
            let declaration = access::declaration(&engine, 1);
            apply(&mut engine, &mut accounting, declaration);
        }
        let (account, amount) = (access::account(1), Fixed::from_int(50));
        let payer = (case != "raw").then_some(account);
        let deposit = AccessDeposit { account: Some(account), payer, amount, chain_ref: "0xpending:1".into() };
        let failed =
            |reason| Command::AccessDeposit { deposit: deposit.clone(), evidence: AccessEvidence::Failed { reason } };
        let ingress = match case {
            "invalid response" => failed(SourceFailure::InvalidResponse),
            "unconfigured" => failed(SourceFailure::NotConfigured),
            "undeclared" => access::funding(&engine, Some(account), payer, amount, &deposit.chain_ref),
            _ => Command::Deposit { account, amount, chain_ref: deposit.chain_ref.clone() },
        };
        apply(&mut engine, &mut accounting, ingress.clone());
        apply(&mut engine, &mut accounting, ingress);
        if case == "raw" {
            assert_eq!(
                (accounting.pending, accounting.held, accounting.liabilities, accounting.deposits),
                (Fixed::ZERO, amount, amount, 1)
            );
        } else {
            assert_eq!(
                (accounting.pending, accounting.liabilities, accounting.deposits),
                (amount, amount, 1),
                "{case}"
            );
            if case == "undeclared" {
                let declaration = access::declaration(&engine, 1);
                apply(&mut engine, &mut accounting, declaration);
            }
            let evidence =
                access::proof(&engine, engine_core::eip712::deposit_screening_hash(&deposit), Some(account), payer);
            let retry = Command::RetryDepositScreening { chain_ref: deposit.chain_ref.clone(), evidence };
            apply(&mut engine, &mut accounting, retry);
            assert_eq!(
                (accounting.pending, accounting.held, accounting.liabilities, accounting.deposits),
                (Fixed::ZERO, Fixed::ZERO, amount, 1),
                "{case}"
            );
            assert_eq!(engine.accounts()[&account].balance, amount);
        }
        let mut replay = Engine::new();
        let mut recovered = Accounting::default();
        for command in log {
            let events = replay.apply(&command).unwrap();
            recovered.apply(&command.cmd, &events).unwrap();
        }
        assert_eq!(recovered, accounting);
    }
}
