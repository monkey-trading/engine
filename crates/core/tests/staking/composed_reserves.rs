//! Reversible principal and outstanding client gains remain reserved together.
use super::*;
use alloy_primitives::B256;
use common::{BTC, Client, params};
use engine_core::access::AccessPolicy;
use engine_core::command::{OrderKind, OrderRequest};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use super::marked_equity::access;

#[test]
fn principal_and_winning_client_pnl_compose_before_claim_reversal_and_settlement() {
    for house in [true, false] {
        for version in [ProtocolVersion::V29, ProtocolVersion::V30] {
            let (mut h, client, receipt, staker) = winning_with_principal(house);
            h.admin(AdminCommand::ActivateProtocol { version });
            next_epoch(&mut h);
            let entitled = if version == ProtocolVersion::V30 { 49_000_000 } else { 99_000_000 };
            let root = h.engine.state_root();
            let proof = h.engine.distribution_proof(staker).unwrap();
            assert!(proof.verify(root.into()));
            assert_eq!(proof.entitled, UsdcUnits(entitled));
            let custody = if house { f("2000") } else { f("2900") };
            let remaining = custody - Fixed::from_raw(entitled as i128 * USDC) - f("900");
            let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
            let events = h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reverse { receipt, observed_at_seq })));
            assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { principal_delta, .. } if *principal_delta == f("-900"))), "{events:?}");
            h.price(BTC, "150");
            close(&mut h, &client);
            assert_eq!(h.balance(client.account), f("1050"));
            assert_eq!(remaining, h.balance(client.account) + h.engine.house_cash());
            if version == ProtocolVersion::V30 {
                assert!(remaining >= h.balance(client.account) + f("1"));
                assert_eq!(h.engine.house_cash(), if house { f("1") } else { f("901") });
            } else if house {
                assert_eq!(h.engine.house_cash(), f("-49"));
            }
        }
    }
}

#[test]
fn v29_to_v30_boundary_combines_principal_before_closing_the_due_epoch() {
    let (mut h, _, _, staker) = winning_with_principal(true);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V29 });
    h.advance_to(config(&h).epoch_end(0));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V30 });
    assert_eq!(h.engine.distribution(staker), UsdcUnits(49_000_000));
}

#[test]
fn principal_and_marked_deficits_compose_without_distributing_the_refund() {
    let (mut h, losing, winning, staker) = super::marked_equity::closed_share_fx_loss_with_house("100");
    let receipt = DepositReceipt {
        block: 3001,
        block_hash: B256::repeat_byte(32),
        tx_hash: B256::repeat_byte(33),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 0,
        beneficiary: DepositBeneficiary::House,
        amount: f("900"),
    };
    let deposit = engine_core::access::receipt_access_deposit(&receipt, Some(winning));
    let evidence = access::evidence(&h, engine_core::eip712::deposit_screening_hash(&deposit), None, Some(winning));
    let events = h.cmd(Command::AccessCustodyDeposit { receipt: receipt.clone(), payer: Some(winning), evidence });
    assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { .. })), "{events:?}");
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V30 });
    next_epoch(&mut h);
    assert_eq!(h.engine.distribution(staker), UsdcUnits(96_000_000));
    let root = h.engine.state_root();
    assert!(h.engine.distribution_proof(staker).unwrap().verify(root.into()));
    let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reverse { receipt, observed_at_seq })));
    super::marked_equity::settle_positions(&mut h, losing, winning);
    assert_eq!(h.engine.house_cash(), f("1"));
    assert_eq!(f("2906") - f("900") - f("96"), h.balance(winning) + f("1"));
}

fn winning_with_principal(house: bool) -> (Harness, Client, DepositReceipt, engine_core::types::AccountId) {
    let mut h = Harness::staking(f("1"), EPOCH_MS, COOLDOWN_MS);
    let mut parameters = params();
    parameters.profit_fee_bps = 0;
    parameters.stress_levels.truncate(1);
    parameters.toxicity.tiers.clear();
    h.operator_admin(AdminCommand::SetParams(parameters));
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy {
        source: engine_core::eip712::address(access::source().verifying_key()),
        max_age_ms: 60_000,
    }));
    h.cmd(Command::HouseCapital {
        amount: if house { f("-99999900") } else { f("-99999000") },
        chain_ref: "initial-house".into(),
    });
    let client = Client::new(1);
    h.deposit(client.account, "1000");
    let receipt = DepositReceipt {
        block: 3000,
        block_hash: B256::repeat_byte(30),
        tx_hash: B256::repeat_byte(31),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 0,
        beneficiary: if house {
            DepositBeneficiary::House
        } else {
            DepositBeneficiary::Trader { account: client.account }
        },
        amount: f("900"),
    };
    let events = h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: receipt.clone() })));
    assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { .. })), "{events:?}");
    let staker = acct(90);
    stake(&mut h, staker, 1, "0xstake:composed");
    h.price(BTC, "100");
    let events = h.market_order(client.account, BTC, Side::Buy, "1", "100");
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    h.price(BTC, "150");
    assert_eq!(h.engine.house_cash(), f("1000"));
    assert_eq!(h.engine.house_nav(), f("950"));
    (h, client, receipt, staker)
}

fn close(h: &mut Harness, previous: &Client) {
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(1) };
    assert_eq!(previous.account, client.account);
    access::attest(h, &client);
    let request = OrderRequest {
        account: client.account,
        client_id: 30,
        market: BTC,
        side: Side::Sell,
        qty: f("1"),
        kind: OrderKind::Market { quoted: f("150"), max_slippage_bps: None },
        reduce_only: true,
        auth: None,
    };
    let Command::PlaceOrder(request) = client.order(h.engine.next_request_nonce(client.account), request) else {
        unreachable!()
    };
    let evidence = access::order_evidence(h, &request);
    let events = h.cmd(Command::AccessOrder { request, evidence });
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
}
