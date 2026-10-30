//! A trading session for the exit tests, built under `V22` and moved to `V36`: a house that starts
//! with no capital, FX markets, and an account holding a position in a USD, a euro, and a yen market,
//! whose withdrawal waits in part in the queue.

#![allow(dead_code)]

use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::market::{MarketSpec, QuoteCurrency};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, Side};

use crate::common::{BTC, DE40, EURUSD, Harness, USDJPY, acct, de40, f, market};

/// `positioned`, then a withdrawal of 150,000 that physical cash covers only in part, under `V22`
/// rules, then `version`. Returns the harness, the account, and the queued request's seq.
pub fn trading_at(version: ProtocolVersion) -> (Harness, AccountId, u64) {
    let (mut h, a) = positioned();
    let queued = withdraw(&mut h, a, "150000");
    h.admin(AdminCommand::ActivateProtocol { version });
    (h, a, queued)
}

/// `trading_at` under `V36`.
pub fn trading() -> (Harness, AccountId, u64) {
    trading_at(ProtocolVersion::V36)
}

/// A `V22` harness, whose client actions need no screening, with a house that starts with no
/// capital and caps floored at a million per market, and an account that deposited 100,000, won
/// 180,000 net of the profit fee on BTC, and holds long 1.333333333 BTC at 20,100, short 2 DE40 at
/// 20,000 euros, and long 1,000 USD-JPY at 150 yen.
pub fn positioned() -> (Harness, AccountId) {
    let mut h = Harness::zero_house_operated(ProtocolVersion::V22);
    let mut params = h.engine.params().unwrap().clone();
    params.house_total_cap_floor = f("10000000");
    h.admin(AdminCommand::SetParams(params));
    let usdjpy = MarketSpec {
        quote_currency: QuoteCurrency::Jpy { fx_market: USDJPY },
        ..market(USDJPY, "USD-JPY", AssetClass::Fx, Schedule::AlwaysOpen, 200)
    };
    let btc = h.engine.markets()[&BTC].spec.clone();
    let eurusd = market(EURUSD, "EUR-USD", AssetClass::Fx, Schedule::AlwaysOpen, 200);
    for spec in [btc, eurusd, usdjpy, de40(500)] {
        h.admin(AdminCommand::UpsertMarket(MarketSpec { house_cap_floor: f("1000000"), ..spec }));
    }
    h.price(EURUSD, "1.1");
    h.price(USDJPY, "150");
    h.price(DE40, "20000");
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100");
    fill(&h.market_order(a, BTC, Side::Buy, "10", "100"));
    h.advance(1_000);
    h.price(BTC, "20100");
    fill(&h.market_order(a, BTC, Side::Sell, "10", "20100"));
    fill(&h.market_order(a, BTC, Side::Buy, "1.333333333", "20100"));
    h.price(EURUSD, "1.1");
    for (market, side, qty, mid) in [(DE40, Side::Sell, "2", "20000"), (USDJPY, Side::Buy, "1000", "150")] {
        h.price(market, mid);
        fill(&h.market_order(a, market, side, qty, mid));
    }
    (h, a)
}

/// Requests a withdrawal of `amount` for `account` and returns its request seq.
pub fn withdraw(h: &mut Harness, account: AccountId, amount: &str) -> u64 {
    let seq = h.log.len() as u64;
    let events = h.cmd(Command::Withdraw { account, amount: f(amount), auth: None });
    assert!(events.iter().any(|e| matches!(e, Event::WithdrawalQueued { .. })), "{events:?}");
    seq
}

/// Checks that exactly one fill applied rather than an order rejection.
pub fn fill(events: &[Event]) {
    assert_eq!(events.iter().filter(|e| matches!(e, Event::Fill(_))).count(), 1, "{events:?}");
}
