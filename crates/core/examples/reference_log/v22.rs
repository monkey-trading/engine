//! The session's `V22` part: withdrawals reserved in a public FIFO queue and paid from physical cash.

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Reject};
use engine_core::market::MarketSpec;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, Side};

use crate::common::{Harness, f, market};

/// A market the part lists for the winning trade that leaves custody short.
const XRP: MarketId = 23;

/// `V22` activates. The large free balances withdraw in full, the house moves out its capital, and a
/// trader then realises a win the remaining physical cash cannot pay: its request is approved in
/// part, keeping its dust, and waits at the head. A deposit approves more of the head, a follower
/// waits behind it, requests below one base unit or beyond free cash are refused, and house capital
/// finishes the head and then the follower.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V22 });
    let large: Vec<(AccountId, Fixed)> = h
        .engine
        .accounts()
        .keys()
        .map(|id| (*id, available(h, *id)))
        .filter(|(_, free)| *free >= f("100000"))
        .collect();
    for (account, free) in large {
        let events = h.cmd(Command::Withdraw { account, amount: free, auth: None });
        assert_eq!(paid(&events), [(account, whole(free))], "{events:?}");
    }

    let spec =
        MarketSpec { max_slippage_bps: 100, ..market(XRP, "XRP-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000) };
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    let [trader, follower] = [35, 36].map(|seed| h.signer(seed));
    h.deposit(trader, "600000");
    h.price(XRP, "1000");
    filled(&h.market_order(trader, XRP, Side::Buy, "5000", "1000"));
    let out = f("1000") - h.engine.house_nav();
    let events = h.cmd(Command::HouseCapital { amount: out, chain_ref: "0xcap:v22-out".into() });
    assert!(events.iter().any(|e| matches!(e, Event::HouseCapitalMoved { .. })), "{events:?}");
    h.advance(1_000);
    h.price(XRP, "1600");
    filled(&h.market_order(trader, XRP, Side::Sell, "5000", "1600"));

    let (balance, cash) = (h.balance(trader), h.engine.withdrawal_cash().expect("V22 counts physical cash"));
    let events = h.cmd(Command::Withdraw { account: trader, amount: balance, auth: None });
    assert_eq!(paid(&events), [(trader, whole(cash))], "{events:?}");
    assert_eq!(h.balance(trader), balance - whole(balance), "dust below one base unit stays");
    let head = h.engine.withdrawal_queue().next().expect("the win leaves custody short").remaining;
    assert!(head.is_positive());

    let events = h.cmd(Command::Deposit { account: follower, amount: f("1000"), chain_ref: "0xdeposit:v22".into() });
    assert_eq!(paid(&events), [(trader, f("1000"))], "{events:?}");
    let events = h.cmd(Command::Withdraw { account: follower, amount: f("0.0000009"), auth: None });
    assert!(matches!(&events[..], [Event::WithdrawalRejected { reason: Reject::InvalidAmount, .. }]), "{events:?}");
    let events = h.cmd(Command::Withdraw { account: follower, amount: f("2000"), auth: None });
    assert!(
        matches!(&events[..], [Event::WithdrawalRejected { reason: Reject::InsufficientFunds { .. }, .. }]),
        "{events:?}"
    );
    let events = h.cmd(Command::Withdraw { account: follower, amount: f("1000"), auth: None });
    assert!(
        paid(&events).is_empty() && events.iter().any(|e| matches!(e, Event::WithdrawalQueued { .. })),
        "{events:?}"
    );
    assert_eq!(h.engine.withdrawal_queue().count(), 2);

    let head = h.engine.withdrawal_queue().next().expect("the head still waits").remaining;
    let events = h.cmd(Command::HouseCapital { amount: head + f("500"), chain_ref: "0xcap:v22-in".into() });
    assert_eq!(paid(&events), [(trader, head), (follower, f("500"))], "{events:?}");
    let events = h.cmd(Command::HouseCapital { amount: f("500"), chain_ref: "0xcap:v22-last".into() });
    assert_eq!(paid(&events), [(follower, f("500"))], "{events:?}");
    assert_eq!(h.engine.withdrawal_queue().count(), 0);
}

/// Free cash a withdrawal may take: the balance, capped by equity above the margin requirement.
fn available(h: &Harness, account: AccountId) -> Fixed {
    let (equity, margin) = h.equity_and_margin(account);
    h.balance(account).min(equity - margin).max(Fixed::ZERO)
}

/// `amount` rounded down to whole USDC base units, as a withdrawal reserves it.
fn whole(amount: Fixed) -> Fixed {
    Fixed::from_raw(amount.raw() / 1_000 * 1_000)
}

/// The approved payouts in event order, by account and amount.
fn paid(events: &[Event]) -> Vec<(AccountId, Fixed)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::WithdrawalApproved { account, amount, .. } => Some((*account, *amount)),
            _ => None,
        })
        .collect()
}

/// Requires the market order to fill rather than be refused.
fn filled(events: &[Event]) {
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
}
