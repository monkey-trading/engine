//! Distributions and immutable withdrawal approvals reserve the same physical collateral.
use super::marked_equity::access;
use super::*;
use common::{BTC, Client, params};
use engine_core::access::AccessPolicy;
use engine_core::command::{OrderKind, OrderRequest, WithdrawalRequest};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

#[test]
fn claimed_distribution_then_actual_position_gain_approves_only_available_fifo_partial() {
    for version in [ProtocolVersion::V30, ProtocolVersion::V31] {
        let (mut h, client, staker) = setup();
        h.admin(AdminCommand::ActivateProtocol { version });
        next_epoch(&mut h);
        let root = h.engine.state_root();
        let proof = h.engine.distribution_proof(staker).unwrap();
        assert!(proof.verify(root.into()));
        assert_eq!(proof.entitled, UsdcUnits(900_000_000));
        let vault_after_claim = f("2000") - f("900");
        close_with_gain(&mut h, &client);
        assert_eq!(h.balance(client.account), f("2000"));
        let events = withdraw(&mut h, &client, "2000");
        let expected = if version == ProtocolVersion::V31 { vault_after_claim } else { f("2000") };
        assert!(
            events.iter().any(|e| matches!(e, Event::WithdrawalApproved { amount, .. } if *amount == expected)),
            "{events:?}"
        );
        assert_eq!(h.engine.withdrawal_approval(client.account, 1).unwrap().amount, expected);
        if version == ProtocolVersion::V31 {
            assert_eq!(h.engine.withdrawal_queue().next().unwrap().remaining, f("900"));
            assert_eq!(h.engine.withdrawal_cash(), Some(Fixed::ZERO));
        }
    }
}

#[test]
fn activation_reconciles_inherited_allocations_once_and_preserves_prior_approvals() {
    let (mut h, client, _) = setup();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V30 });
    next_epoch(&mut h);
    assert_eq!(h.engine.withdrawal_cash(), Some(f("2000")));
    close_with_gain(&mut h, &client);
    withdraw(&mut h, &client, "1500");
    let approval = *h.engine.withdrawal_approval(client.account, 1).unwrap();
    assert_eq!(h.engine.withdrawal_cash(), Some(f("500")));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V31 });
    assert_eq!(h.engine.withdrawal_cash(), Some(f("-400")));
    assert_eq!(h.engine.withdrawal_approval(client.account, 1), Some(&approval));
    let events = withdraw(&mut h, &client, "500");
    assert!(!events.iter().any(|e| matches!(e, Event::WithdrawalApproved { .. })), "{events:?}");
    let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V31 }));
    assert!(events.iter().any(|e| matches!(e, Event::AdminRejected { .. })));
    assert_eq!(h.engine.withdrawal_cash(), Some(f("-400")));
    h.cmd(Command::Tick);
    assert_eq!(h.engine.withdrawal_cash(), Some(f("-400")));
}

#[test]
fn activation_at_epoch_end_reserves_old_and_new_allocations_exactly_once() {
    let (mut h, _, staker) = setup();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V30 });
    h.advance_to(config(&h).epoch_end(0));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V31 });
    assert_eq!(h.engine.distribution(staker), UsdcUnits(900_000_000));
    assert_eq!(h.engine.withdrawal_cash(), Some(f("1100")));
    next_epoch(&mut h);
    assert_eq!(h.engine.withdrawal_cash(), Some(f("1100")));
}

#[test]
fn receipt_reversal_never_releases_distribution_reserves_and_finality_releases_only_principal() {
    use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
    let (mut h, client, staker) = setup();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V31 });
    next_epoch(&mut h);
    let receipt = DepositReceipt {
        block: 3100,
        block_hash: alloy_primitives::B256::repeat_byte(31),
        tx_hash: alloy_primitives::B256::repeat_byte(32),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 0,
        beneficiary: DepositBeneficiary::House,
        amount: f("500"),
    };
    let deposit = engine_core::access::receipt_access_deposit(&receipt, Some(client.account));
    let evidence = access::evidence(&h, engine_core::eip712::deposit_screening_hash(&deposit), None, deposit.payer);
    h.cmd(Command::AccessCustodyDeposit { receipt: receipt.clone(), payer: deposit.payer, evidence });
    assert_eq!(h.engine.withdrawal_cash(), Some(f("1100")));
    let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reverse { receipt: receipt.clone(), observed_at_seq })));
    assert_eq!(h.engine.withdrawal_cash(), Some(f("1100")));
    let reversed_at_seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reinstate { receipt: receipt.clone(), reversed_at_seq })));
    assert_eq!(h.engine.withdrawal_cash(), Some(f("1100")));
    let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Finalise { receipt, observed_at_seq })));
    assert_eq!(h.engine.withdrawal_cash(), Some(f("1600")));
    next_epoch(&mut h);
    assert_eq!(h.engine.distribution(staker), UsdcUnits(1_400_000_000));
    assert_eq!(h.engine.withdrawal_cash(), Some(f("1100")));
}

#[test]
fn rejected_v31_crossing_keeps_the_due_epoch_pending_until_valid_activation() {
    let (mut h, _, staker) = setup();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V30 });
    h.advance_to(config(&h).epoch_end(0));
    let operator = Client { expires_at: h.ts + 60_000, ..Client::operator() };
    let mut command = operator.admin(
        h.engine.next_request_nonce(operator.account),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::V31 },
    );
    let Command::SignedAdmin(request) = &mut command else { unreachable!() };
    request.auth.signature = engine_core::eip712::Signature::ZERO;
    let events = h.cmd(command);
    assert!(events.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { .. })), "{events:?}");
    assert_eq!(h.engine.staking().unwrap().epoch, 0);
    assert_eq!(h.engine.distribution(staker), UsdcUnits(0));
    assert_eq!(h.engine.withdrawal_cash(), Some(f("2000")));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V31 });
    assert_eq!(h.engine.distribution(staker), UsdcUnits(900_000_000));
    assert_eq!(h.engine.withdrawal_cash(), Some(f("1100")));
}

#[test]
fn new_distribution_preserves_existing_approval_capacity_whether_paid_or_unpaid() {
    let (mut h, client, staker) = setup();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V31 });
    access::attest(&mut h, &client);
    let events = withdraw(&mut h, &client, "500");
    assert!(
        events.iter().any(|e| matches!(e, Event::WithdrawalApproved { amount, .. } if *amount == f("500"))),
        "{events:?}"
    );
    let approval = *h.engine.withdrawal_approval(client.account, 1).unwrap();
    assert_eq!(h.engine.withdrawal_cash(), Some(f("1500")));
    next_epoch(&mut h);
    assert_eq!(h.engine.distribution(staker), UsdcUnits(900_000_000));
    assert_eq!(h.engine.withdrawal_cash(), Some(f("600")));
    assert_eq!(h.engine.withdrawal_approval(client.account, 1), Some(&approval));
    assert_eq!(f("2000") - approval.amount - f("900"), h.balance(client.account) + f("100"));
}

fn setup() -> (Harness, Client, engine_core::types::AccountId) {
    let mut h = Harness::staking(f("100"), EPOCH_MS, COOLDOWN_MS);
    let mut parameters = params();
    parameters.profit_fee_bps = 0;
    parameters.stress_levels.truncate(1);
    parameters.toxicity.tiers.clear();
    parameters.house_total_cap_floor = f("10000");
    h.operator_admin(AdminCommand::SetParams(parameters));
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy {
        source: engine_core::eip712::address(access::source().verifying_key()),
        max_age_ms: 60_000,
    }));
    h.cmd(Command::HouseCapital { amount: f("-99999000"), chain_ref: "house-1000".into() });
    let client = Client::new(1);
    h.deposit(client.account, "1000");
    let staker = acct(90);
    stake(&mut h, staker, 1, "0xstake:withdrawal-capacity");
    h.price(BTC, "100");
    let events = h.market_order(client.account, BTC, Side::Buy, "1", "100");
    assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
    (h, client, staker)
}

fn close_with_gain(h: &mut Harness, old_client: &Client) {
    h.price(BTC, "1100");
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(1) };
    assert_eq!(client.account, old_client.account);
    access::attest(h, &client);
    let request = OrderRequest {
        account: client.account,
        client_id: 31,
        market: BTC,
        side: Side::Sell,
        qty: f("1"),
        kind: OrderKind::Market { quoted: f("1100"), max_slippage_bps: None },
        reduce_only: true,
        auth: None,
    };
    let Command::PlaceOrder(request) = client.order(h.engine.next_request_nonce(client.account), request) else {
        unreachable!()
    };
    let evidence = access::order_evidence(h, &request);
    let events = h.cmd(Command::AccessOrder { request, evidence });
    assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
}

fn withdraw(h: &mut Harness, old_client: &Client, amount: &str) -> Vec<Event> {
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(1) };
    assert_eq!(client.account, old_client.account);
    let Command::Withdraw { account, amount, auth } =
        client.withdraw(h.engine.next_request_nonce(client.account), amount)
    else {
        unreachable!()
    };
    let request = WithdrawalRequest { account, amount, auth };
    let auth = request.auth.as_ref().unwrap();
    let hash = engine_core::eip712::withdrawal_hash(&client.domain, account, amount, auth.nonce, auth.expires_at);
    let evidence = access::evidence(h, hash, Some(account), None);
    h.cmd(Command::AccessWithdraw { request, evidence, payout_available: true })
}
