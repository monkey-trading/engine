//! Local V22 test configuration and commands; no protocol reference fixture is generated.

#![allow(dead_code)]

use crate::common::{BTC, ETH, Harness, acct, f};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};
use engine_core::{Engine, Fixed};

/// V17 with simple fee-free fills and large fixed house caps, before V22 migration.
pub fn predecessor() -> Harness {
    let mut h = Harness::v17();
    let mut params = h.engine.params().unwrap().clone();
    params.profit_fee_bps = 0;
    params.stress_levels.truncate(1);
    params.toxicity.tiers.iter_mut().for_each(|tier| tier.extra_spread_bps = 0);
    params.house_total_cap_floor = f("1000000");
    h.operator_admin(AdminCommand::SetParams(params));
    for market in [BTC, ETH] {
        let mut spec = h.engine.markets()[&market].spec.clone();
        spec.house_cap_floor = f("1000000");
        h.operator_admin(AdminCommand::UpsertMarket(spec));
    }
    capital(&mut h, "-99999990");
    h
}

/// A local explicit V22 activation, with ten dollars of physical house capital.
pub fn v22() -> Harness {
    let mut h = predecessor();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V22 });
    assert_eq!(h.engine.withdrawal_cash(), Some(f("10")));
    h
}

/// A custody credit whose command may also approve pending withdrawals.
pub fn deposit(h: &mut Harness, account: AccountId, amount: &str) -> Vec<Event> {
    let chain_ref = format!("withdrawal-deposit-{}", h.log.len());
    let events = h.cmd(Command::Deposit { account, amount: f(amount), chain_ref });
    assert!(events.contains(&Event::Deposited { account, amount: f(amount) }), "{events:?}");
    events
}

/// A house capital movement with a unique test custody reference.
pub fn capital(h: &mut Harness, amount: &str) -> Vec<Event> {
    let chain_ref = format!("withdrawal-capital-{}", h.log.len());
    let events = h.cmd(Command::HouseCapital { amount: f(amount), chain_ref });
    assert!(events.iter().any(|e| matches!(e, Event::HouseCapitalMoved { .. })), "{events:?}");
    events
}

/// A fee-free round trip that realises one hundred dollars, without adding physical cash.
pub fn win(h: &mut Harness, account: AccountId) {
    h.price(BTC, "100");
    assert_filled(&h.market_order(account, BTC, Side::Buy, "1", "100"));
    h.price(BTC, "200");
    assert_filled(&h.market_order(account, BTC, Side::Sell, "1", "200"));
}

/// Requests a withdrawal through the same sequenced command used by the node.
pub fn withdraw(h: &mut Harness, account: AccountId, amount: &str) -> Vec<Event> {
    h.cmd(Command::Withdraw { account, amount: f(amount), auth: None })
}

/// The payouts in event order, without the other public queue and market events.
pub fn approvals(events: &[Event]) -> Vec<(AccountId, Fixed, u64)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::WithdrawalApproved { account, amount, nonce } => Some((*account, *amount, *nonce)),
            _ => None,
        })
        .collect()
}

/// Two winning accounts, with thirty physical dollars and two fully reserved withdrawal requests.
pub fn two_waiting() -> Harness {
    let mut h = v22();
    deposit(&mut h, acct(1), "10");
    deposit(&mut h, acct(2), "10");
    win(&mut h, acct(1));
    win(&mut h, acct(2));
    assert_eq!(approvals(&withdraw(&mut h, acct(1), "110")), [(acct(1), f("30"), 1)]);
    assert!(approvals(&withdraw(&mut h, acct(2), "110")).is_empty());
    h
}

/// Checks that exactly one fill applied rather than an order rejection.
pub fn assert_filled(events: &[Event]) {
    assert_eq!(events.iter().filter(|e| matches!(e, Event::Fill(_))).count(), 1, "{events:?}");
}

/// Cash backing both unreserved accounts and queue reservations.
pub fn cash_and_reservations(engine: &Engine) -> Fixed {
    engine.house_cash()
        + engine.accounts().values().map(|a| a.balance).sum::<Fixed>()
        + engine.withdrawal_queue().map(|q| q.remaining).sum::<Fixed>()
}
