//! Unfunded realised losses remain reserved while another position cannot close.

use super::*;
use crate::common::{BTC, ETH, params};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

#[test]
fn v28_never_distributes_other_clients_collateral_while_a_partial_liquidation_leaves_negative_cash() {
    for version in [ProtocolVersion::V25, ProtocolVersion::V27, ProtocolVersion::V28] {
        let (mut h, trader, other, staker) = partial_liquidation();
        if version != ProtocolVersion::V25 {
            h.admin(AdminCommand::ActivateProtocol { version });
        }
        let events = next_epoch(&mut h);
        let expected = if version >= ProtocolVersion::V28 { 2_100_000_000 } else { 2_890_000_000 };
        assert_eq!(h.engine.distribution(staker), UsdcUnits(expected), "{version:?}: {events:?}");
        assert_eq!(h.balance(trader), f("-790"));
        assert_eq!(h.balance(other), f("1000"));
        let root = h.engine.state_root();
        let proof = h.engine.distribution_proof(staker).unwrap();
        assert_eq!(proof.entitled, UsdcUnits(expected));
        assert!(proof.verify(root.into()));
        if version >= ProtocolVersion::V28 {
            // The vault began with 2000 house + 200 losing-client + 1000 other-client USDC.
            let claimed = Fixed::from_raw(expected as i128 * USDC);
            assert_eq!(f("3200") - claimed, h.balance(other) + f("100"));
            assert_eq!(h.engine.house_cash() + h.balance(trader), f("100"));
        }
    }
}

#[test]
fn settlement_of_the_remaining_position_releases_the_reserve_without_distributing_the_loss_twice() {
    let (mut h, trader, other, staker) = partial_liquidation();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V28 });
    next_epoch(&mut h);
    let events = h.price(ETH, "100");
    assert!(
        events.iter().any(
            |event| matches!(event, Event::BadDebt { account, amount } if *account == trader && *amount == f("790"))
        ),
        "{events:?}"
    );
    assert!(h.engine.accounts()[&trader].positions.is_empty());
    assert_eq!(h.balance(trader), Fixed::ZERO);
    assert_eq!(h.balance(other), f("1000"));
    assert_eq!(h.engine.house_cash(), f("100"));
    let events = next_epoch(&mut h);
    assert!(
        events.iter().any(|event| matches!(event, Event::DistributionEpochClosed { allocated: UsdcUnits(0), .. })),
        "{events:?}"
    );
    assert_eq!(h.engine.distribution(staker), UsdcUnits(2_100_000_000));
}

#[test]
fn negative_cash_and_unfinalised_principal_are_reserved_independently_until_finality() {
    use alloy_primitives::B256;
    use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
    let (mut h, trader, other, staker) = partial_liquidation();
    let receipt = DepositReceipt {
        block: 2000,
        block_hash: B256::repeat_byte(28),
        tx_hash: B256::repeat_byte(29),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 0,
        beneficiary: DepositBeneficiary::House,
        amount: f("500"),
    };
    let events = h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: receipt.clone() })));
    assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { principal_delta, .. } if *principal_delta == f("500"))), "{events:?}");
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V28 });
    next_epoch(&mut h);
    assert_eq!(
        h.engine.distribution(staker),
        UsdcUnits(2_100_000_000),
        "unfinalised house principal cannot increase the claim"
    );
    let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Finalise { receipt, observed_at_seq })));
    next_epoch(&mut h);
    assert_eq!(h.engine.distribution(staker), UsdcUnits(2_600_000_000), "only finalised principal becomes available");
    assert_eq!(h.balance(trader), f("-790"));
    assert_eq!(h.balance(other), f("1000"));
    assert_eq!(h.engine.house_cash() + h.balance(trader), f("100"));
}

/// Real trades leave the liquidated account negative while its other market has no fresh quote.
fn partial_liquidation()
-> (Harness, engine_core::types::AccountId, engine_core::types::AccountId, engine_core::types::AccountId) {
    let mut h = Harness::staking(f("100"), EPOCH_MS, COOLDOWN_MS);
    let mut parameters = params();
    parameters.profit_fee_bps = 0;
    parameters.stress_levels.truncate(1);
    h.operator_admin(AdminCommand::SetParams(parameters));
    let events = h.cmd(Command::HouseCapital { amount: f("-99998000"), chain_ref: "house-to-2000".into() });
    assert!(events.iter().any(|event| matches!(event, Event::HouseCapitalMoved { .. })), "{events:?}");
    let (trader, other, staker) = (acct(1), acct(2), acct(90));
    h.deposit(trader, "200");
    h.deposit(other, "1000");
    stake(&mut h, staker, 1, "0xstake:negative-cash");
    h.price(BTC, "100");
    h.price(ETH, "100");
    for (market, quantity) in [(BTC, "10"), (ETH, "1")] {
        let events = h.market_order(trader, market, Side::Buy, quantity, "100");
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    }
    h.advance(2_000);
    let events = h.price(BTC, "1");
    assert!(
        events.iter().any(|event| matches!(event, Event::CloseOut { account, .. } if *account == trader)),
        "{events:?}"
    );
    assert!(h.engine.accounts()[&trader].position(BTC).qty.is_zero());
    assert_eq!(h.engine.accounts()[&trader].position(ETH).qty, f("1"));
    assert_eq!(h.balance(trader), f("-790"));
    assert_eq!(h.engine.house_cash(), f("2990"));
    assert!(
        !events.iter().any(|event| matches!(event, Event::BadDebt { .. })),
        "the unresolved position prevents settlement"
    );
    (h, trader, other, staker)
}
