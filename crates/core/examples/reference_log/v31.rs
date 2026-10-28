//! V31 reserves distributions from physical payout capacity, including inherited allocations.
use crate::common::{Client, Harness, f};
use engine_core::access::{AccessDeposit, AccessEvidence, Jurisdiction, SanctionsStatus, Screening};
use engine_core::command::{AdminCommand, Command};
use engine_core::eip712::{self, B256, Signature};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;

/// Migrates historical allocation reserves and debits subsequent allocations exactly once.
pub(crate) fn session(h: &mut Harness) {
    let payer = Client::new(83).account;
    fund(h, None, payer, "2000000", "0xv31:final-house");
    let capacity = h.engine.withdrawal_cash().unwrap();
    let inherited = h.engine.staking().unwrap().distributed;
    assert!(inherited.0 > 0);
    let end = {
        let staking = h.engine.staking().unwrap();
        staking.config.epoch_end(staking.epoch)
    };
    h.advance_to(end);
    let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V31 }));
    assert!(events.iter().any(|event| matches!(event, Event::AdminApplied)), "{events:?}");
    let distributed = h.engine.staking().unwrap().distributed;
    assert!(distributed.0 > inherited.0, "new version must allocate at its activation boundary");
    let reserved = engine_core::withdrawal::from_usdc(distributed.0).unwrap();
    assert_eq!(h.engine.withdrawal_cash(), Some(capacity - reserved));
    let root = h.engine.state_root();
    let staker = h.signer(80);
    assert!(h.engine.distribution_proof(staker).unwrap().verify(root.into()));
    let capacity = h.engine.withdrawal_cash();
    let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V31 }));
    assert!(events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    assert_eq!(h.engine.withdrawal_cash(), capacity);
    let end = {
        let staking = h.engine.staking().unwrap();
        staking.config.epoch_end(staking.epoch)
    };
    h.advance_to(end);
    h.cmd(Command::Tick);
    let newly_reserved =
        engine_core::withdrawal::from_usdc(h.engine.staking().unwrap().distributed.0 - distributed.0).unwrap();
    assert_eq!(h.engine.withdrawal_cash(), capacity.map(|cash| cash - newly_reserved));
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
