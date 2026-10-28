//! V30 composes reversible house and trader principal with marked client liabilities.
use crate::common::{Client, Harness, f};
use engine_core::access::{AccessDeposit, AccessEvidence, Jurisdiction, SanctionsStatus, Screening};
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712::{self, B256, Signature};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;

/// Keeps both principal types locked while the previous session still has marked gains and deficits.
pub(crate) fn session(h: &mut Harness) {
    let payer = Client::new(83).account;
    fund(h, None, payer, "1000000", "0xv30:final-house");
    let mut receipts = Vec::new();
    for (index, beneficiary) in
        [DepositBeneficiary::House, DepositBeneficiary::Trader { account: payer }].into_iter().enumerate()
    {
        let receipt = DepositReceipt {
            block: 30_000 + index as u64,
            block_hash: B256::repeat_byte(0x90 + index as u8),
            tx_hash: B256::repeat_byte(0x92 + index as u8),
            log_index: 0,
            transaction_deposit_index: 0,
            deposit_id: 0,
            beneficiary,
            amount: f("900"),
        };
        let deposit = engine_core::access::receipt_access_deposit(&receipt, Some(payer));
        let evidence = screened(h, eip712::deposit_screening_hash(&deposit), deposit.account, deposit.payer);
        let events = h.cmd(Command::AccessCustodyDeposit { receipt: receipt.clone(), payer: Some(payer), evidence });
        assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { principal_delta, .. } if *principal_delta == f("900"))), "{events:?}");
        receipts.push(receipt);
    }
    let cash = h.engine.house_cash().raw();
    let nav = h.engine.house_nav().raw();
    let deficit: i128 =
        h.engine.accounts().values().map(|a| h.engine.equity_and_margin(a, None).0.raw().saturating_neg().max(0)).sum();
    let client_principal: i128 = h.engine.accounts().values().map(|a| a.unfinalised_deposits.raw()).sum();
    let debt: i128 = h.engine.accounts().values().map(|a| a.balance.raw().saturating_neg().max(0)).sum();
    let principal = f("900").raw() + client_principal;
    let floor = h.engine.staking().unwrap().config.liquidity_floor.raw();
    let funded = cash - principal - debt;
    let expected = (nav - principal - deficit).min(funded).saturating_sub(floor).max(0) as u128 / 1_000;
    let legacy = (nav - deficit).min(funded).saturating_sub(floor).max(0) as u128 / 1_000;
    assert!(
        expected < legacy,
        "principal cap expected={expected} legacy={legacy} nav={nav} cash={cash} principal={principal} deficit={deficit} debt={debt} floor={floor}"
    );
    let staking = h.engine.staking().unwrap();
    let before = staking.distributed;
    h.advance_to(staking.config.epoch_end(staking.epoch));
    let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V30 }));
    assert!(events.iter().any(|event| matches!(event, Event::AdminApplied)), "{events:?}");
    assert_eq!(h.engine.staking().unwrap().distributed.0 - before.0, expected);
    let root = h.engine.state_root();
    let staker = h.signer(80);
    assert!(h.engine.distribution_proof(staker).unwrap().verify(root.into()));
    for receipt in receipts {
        let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
        let events = h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reverse { receipt, observed_at_seq })));
        assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { principal_delta, .. } if *principal_delta == f("-900"))), "{events:?}");
    }
}

/// Supplies signed source evidence to funding, declarations and orders.
fn screened(h: &Harness, request_hash: B256, account: Option<AccountId>, payer: Option<AccountId>) -> AccessEvidence {
    let source = SigningKey::from_slice(&[0x77; 32]).unwrap();
    let mut screening = Screening {
        source: eip712::address(source.verifying_key()),
        request_hash,
        observed_at: h.ts,
        account,
        payer,
        jurisdiction: Jurisdiction::Allowed,
        account_status: SanctionsStatus::Clear,
        payer_status: SanctionsStatus::Clear,
        signature: Signature::ZERO,
    };
    screening.signature =
        eip712::sign(&source, &eip712::screening_hash(&h.engine.access_domain().unwrap(), &screening));
    AccessEvidence::Screened(screening)
}

/// Funds the house or records a new client's pending deposit before its declaration.
fn fund(h: &mut Harness, account: Option<AccountId>, payer: AccountId, amount: &str, reference: &str) {
    let deposit = AccessDeposit { account, payer: Some(payer), amount: f(amount), chain_ref: reference.into() };
    let evidence = screened(h, eip712::deposit_screening_hash(&deposit), account, Some(payer));
    let events = h.cmd(Command::AccessDeposit { deposit, evidence });
    assert!(
        events.iter().any(|event| matches!(event, Event::HouseCapitalMoved { .. } | Event::DepositPending { .. })),
        "{events:?}"
    );
}
