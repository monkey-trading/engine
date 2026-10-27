//! Closed-share FX changes cannot turn uncollectible client losses into staker claims.

use super::*;
use crate::common::{Client, EURUSD, market, ny, us_equities};
use engine_core::access::AccessPolicy;
use engine_core::auth::ClientAuth;
use engine_core::command::{OrderKind, OrderRequest};
use engine_core::eip712;
use engine_core::market::QuoteCurrency;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, Side};

#[allow(dead_code)]
#[path = "../access/support.rs"]
pub(super) mod access;

const SHARE: u32 = 290;

#[test]
fn v29_discounts_negative_marked_equity_with_positive_cash_before_clients_settle() {
    for version in [ProtocolVersion::V28, ProtocolVersion::V29] {
        let (mut h, losing, winning, staker) = closed_share_fx_loss();
        if version == ProtocolVersion::V29 {
            h.admin(AdminCommand::ActivateProtocol { version });
        }
        let events = next_epoch(&mut h);
        let expected = if version == ProtocolVersion::V29 { 996_000_000 } else { 999_000_000 };
        assert_eq!(h.engine.distribution(staker), UsdcUnits(expected), "{events:?}");
        let root = h.engine.state_root();
        let proof = h.engine.distribution_proof(staker).unwrap();
        assert!(proof.verify(root.into()));
        let paid = Fixed::from_raw(proof.entitled.0 as i128 * USDC);
        let remaining = f("2906") - paid;
        let winning_equity = h.equity_and_margin(winning).0;
        assert_eq!(winning_equity, f("1909"));
        if version == ProtocolVersion::V29 {
            assert_eq!(remaining, winning_equity + f("1"));
        } else {
            assert!(remaining < winning_equity);
        }
        settle_positions(&mut h, losing, winning);
        assert_eq!(h.balance(winning), f("1909"));
        assert_eq!(h.balance(losing), Fixed::ZERO);
        assert_eq!(h.engine.house_cash(), if version == ProtocolVersion::V29 { f("1") } else { f("-2") });
        assert_eq!(h.balance(winning) + h.engine.house_cash(), remaining);
    }
}

#[test]
fn marked_equity_and_cash_debt_caps_do_not_charge_the_same_deficit_twice() {
    for version in [ProtocolVersion::V29, ProtocolVersion::V30] {
        let (mut h, losing, other, staker) = super::negative_cash::partial_liquidation();
        h.admin(AdminCommand::ActivateProtocol { version });
        next_epoch(&mut h);
        assert_eq!(h.engine.distribution(staker), UsdcUnits(2_100_000_000));
        assert_eq!(h.engine.house_cash() + h.balance(losing), f("100"));
        assert_eq!(h.balance(other), f("1000"));
    }
}

#[test]
fn crossing_from_v28_to_v29_at_epoch_end_uses_the_collectible_equity_cap_immediately() {
    let (mut h, _, winning, staker) = closed_share_fx_loss();
    h.advance_to(config(&h).epoch_end(0));
    let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V29 }));
    assert!(events.iter().any(|event| matches!(event, Event::AdminApplied)), "{events:?}");
    assert_eq!(h.engine.distribution(staker), UsdcUnits(996_000_000));
    assert_eq!(f("2906") - f("996"), h.equity_and_margin(winning).0 + f("1"));
}

/// Two offsetting euro positions leave reported NAV flat while a closed-market FX move makes one insolvent.
pub(super) fn closed_share_fx_loss() -> (Harness, AccountId, AccountId, AccountId) {
    closed_share_fx_loss_with_house("1000")
}

pub(super) fn closed_share_fx_loss_with_house(house: &str) -> (Harness, AccountId, AccountId, AccountId) {
    let mut h = Harness::v8(ClientAuth::Eip712 { chain_id: crate::common::CHAIN_ID, vault: crate::common::VAULT });
    let mut parameters = h.engine.params().unwrap().clone();
    parameters.profit_fee_bps = 0;
    parameters.house_total_cap_floor = f("10000");
    parameters.stress_levels.truncate(1);
    parameters.toxicity.tiers.clear();
    h.operator_admin(AdminCommand::SetParams(parameters));
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy {
        source: eip712::address(access::source().verifying_key()),
        max_age_ms: 60_000,
    }));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V25 });
    h.operator_admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 0 });
    h.operator_admin(AdminCommand::UpsertMarket(market(EURUSD, "EUR-USD", AssetClass::Fx, Schedule::AlwaysOpen, 200)));
    let mut share = market(SHARE, "EUR-SHARE", AssetClass::Equity, us_equities(), 1_000);
    share.quote_currency = QuoteCurrency::Eur { fx_market: EURUSD };
    share.house_cap_floor = f("10000");
    share.financing.markup_bps = 0;
    h.operator_admin(AdminCommand::UpsertMarket(share));
    h.cmd(Command::HouseCapital { amount: f(house) - f("100000000"), chain_ref: "house-to-1000".into() });
    let closes = ny(2026, 10, 20, 16, 0);
    h.operator_admin(AdminCommand::InitStaking(StakingConfig {
        liquidity_floor: f("1"),
        epoch_ms: EPOCH_MS,
        start_ms: closes,
        cooldown_ms: COOLDOWN_MS,
    }));
    let (losing, winning, staker) = (h.signer(1), h.signer(2), acct(90));
    h.deposit(losing, "906");
    h.deposit(winning, "1000");
    stake(&mut h, staker, 1, "0xstake:marked-equity");
    h.price(EURUSD, "1");
    h.price(SHARE, "1000");
    for (account, side) in [(losing, Side::Buy), (winning, Side::Sell)] {
        let events = h.market_order(account, SHARE, side, "1", "1000");
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    }
    h.price(SHARE, "100");
    assert_eq!(h.equity_and_margin(losing).0, f("6"));
    h.advance_to(closes);
    h.price(EURUSD, "1.01");
    assert_eq!(h.balance(losing), f("906"));
    assert_eq!(h.equity_and_margin(losing).0, f("-3"));
    assert_eq!(h.engine.accounts()[&losing].position(SHARE).qty, f("1"));
    assert!(!h.engine.markets()[&SHARE].open);
    assert_eq!(h.engine.house_cash(), f(house));
    assert_eq!(h.engine.house_nav(), f(house));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V28 });
    (h, losing, winning, staker)
}

/// Reopening settles the insolvent long and then the solvent short through an authenticated order.
pub(super) fn settle_positions(h: &mut Harness, losing: AccountId, winning: AccountId) {
    h.advance_to(ny(2026, 10, 21, 9, 30));
    h.price(EURUSD, "1.01");
    let events = h.price(SHARE, "100");
    assert!(
        events.iter().any(
            |event| matches!(event, Event::BadDebt { account, amount } if *account == losing && *amount == f("3"))
        ),
        "{events:?}"
    );
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(2) };
    assert_eq!(client.account, winning);
    access::attest(h, &client);
    let request = OrderRequest {
        account: winning,
        client_id: 29,
        market: SHARE,
        side: Side::Buy,
        qty: f("1"),
        kind: OrderKind::Market { quoted: f("100"), max_slippage_bps: None },
        reduce_only: true,
        auth: None,
    };
    let Command::PlaceOrder(request) = client.order(h.engine.next_request_nonce(winning), request) else {
        unreachable!()
    };
    let evidence = access::order_evidence(h, &request);
    let events = h.cmd(Command::AccessOrder { request, evidence });
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    assert!(h.engine.accounts()[&winning].positions.is_empty());
}
