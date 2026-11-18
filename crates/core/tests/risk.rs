//! Risk: margin, close-out, negative balance protection, withdrawals, house exposure caps, and
//! reduce-only, end to end through `Engine::apply`.

mod common;

use common::events::{fills, rejection};
use common::{BTC, ETH, Harness, acct, f, params};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::params::ProtocolParams;
use engine_core::types::Side;

#[test]
fn orders_beyond_the_margin_the_account_can_post_are_rejected() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "1000");
    h.price(BTC, "100000");
    // 0.11 BTC is 11,000 notional at 10% margin: 1,100 required against 1,000 of equity.
    let ev = h.market_order(a, BTC, Side::Buy, "0.11", "100000");
    assert!(matches!(rejection(&ev), Reject::InsufficientMargin { .. }));
    assert!(h.engine.accounts()[&a].positions.is_empty());
    // 0.1 BTC needs exactly 1,000: allowed.
    assert_eq!(fills(&h.market_order(a, BTC, Side::Buy, "0.1", "100000")).len(), 1);
}

#[test]
fn closeout_takes_the_largest_loser_first() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "1000");
    h.price(BTC, "100000");
    h.price(ETH, "4000");
    h.market_order(a, BTC, Side::Buy, "0.04", "100000");
    h.market_order(a, ETH, Side::Buy, "1", "4000");

    h.price(BTC, "97000");
    h.price(ETH, "3500");
    assert_eq!(h.engine.accounts()[&a].positions.len(), 2);

    // Equity 280 < 50% of margin 728: ETH (-600) goes, BTC (-120) survives.
    let ev = h.price(ETH, "3400");
    assert!(ev.iter().any(|e| matches!(e, Event::CloseOut { .. })));
    let closed = fills(&ev);
    assert_eq!(closed.len(), 1);
    assert_eq!((closed[0].market, closed[0].liquidation), (ETH, true));
    assert!(h.engine.accounts()[&a].positions.contains_key(&BTC));
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn negative_balance_protection_writes_off_gaps_against_the_house() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "1000");
    h.price(BTC, "100000");
    h.market_order(a, BTC, Side::Buy, "0.09", "100000");

    // A 20% gap also trips stress, so the close-out pays the 5bps stress
    // spread: sold at 79,960 for a loss of 1,803.6 on a 1,000 balance.
    let ev = h.price(BTC, "80000");
    assert!(ev.contains(&Event::BadDebt { account: a, amount: f("803.6") }), "{ev:?}");
    assert_eq!(h.balance(a), Fixed::ZERO);
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn withdrawals_are_limited_to_free_cash() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "1000");
    h.price(BTC, "100000");
    h.market_order(a, BTC, Side::Buy, "0.09", "100000");

    let ev = h.cmd(Command::Withdraw { account: a, amount: f("200"), auth: None });
    assert!(
        matches!(&ev[0], Event::WithdrawalRejected { reason: Reject::InsufficientFunds { available }, .. } if *available == f("100"))
    );
    let ev = h.cmd(Command::Withdraw { account: a, amount: f("100"), auth: None });
    assert_eq!(ev[0], Event::WithdrawalApproved { account: a, amount: f("100"), nonce: 1 });
    h.assert_conserved(f("100"));
}

#[test]
fn withdrawals_of_out_of_range_amounts_are_rejected() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "1000");
    for amount in ["0", "-5", "1000000000.000000001"] {
        let ev = h.cmd(Command::Withdraw { account: a, amount: f(amount), auth: None });
        assert!(matches!(&ev[0], Event::WithdrawalRejected { reason: Reject::InvalidAmount, .. }), "{amount}");
    }
    assert_eq!(h.balance(a), f("1000"));
}

#[test]
fn house_exposure_is_capped_but_risk_reducing_flow_is_not() {
    let mut h = Harness::new();
    let (whale, other) = (acct(1), acct(2));
    h.deposit(whale, "100000000");
    h.deposit(other, "100000000");
    h.price(BTC, "100000");

    // Cap is 50% of 100m NAV: 60m of BTC is too much.
    let ev = h.market_order(whale, BTC, Side::Buy, "600", "100100");
    assert_eq!(rejection(&ev), &Reject::HouseCapacity);
    let ev = h.market_order(whale, BTC, Side::Buy, "400", "100100");
    assert_eq!(fills(&ev).len(), 1);
    // Taking the other side shrinks house exposure, so size is fine.
    let ev = h.market_order(other, BTC, Side::Sell, "600", "99900");
    assert_eq!(fills(&ev).len(), 1);
}

#[test]
fn the_total_house_cap_binds_across_markets() {
    let mut h = Harness::new();
    let whale = acct(1);
    h.deposit(whale, "100000000");
    // Lower the total cap to 60% of NAV while each market keeps its 50% cap.
    h.admin(AdminCommand::SetParams(ProtocolParams { house_total_cap_bps: 6_000, ..params() }));
    h.price(BTC, "100000");
    h.price(ETH, "4000");
    // 40m of BTC fits both caps.
    assert_eq!(fills(&h.market_order(whale, BTC, Side::Buy, "400", "100100")).len(), 1);
    // 30m of ETH fits ETH's own cap, but 70m in total is past 60m.
    let ev = h.market_order(whale, ETH, Side::Buy, "7500", "4005");
    assert_eq!(rejection(&ev), &Reject::HouseCapacity);
}

#[test]
fn reduce_only_orders_and_markets_never_add_risk() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");

    // A reduce-only order with nothing to reduce is refused.
    let reduce = |side, qty: &str| {
        Command::PlaceOrder(OrderRequest {
            account: a,
            client_id: 1,
            market: BTC,
            side,
            qty: f(qty),
            kind: OrderKind::Market { quoted: f("100000"), max_slippage_bps: None },
            reduce_only: true,
            auth: None,
            take_profit: None,
            stop_loss: None,
        })
    };
    assert_eq!(rejection(&h.cmd(reduce(Side::Sell, "0.1"))), &Reject::ReduceOnly);

    // Against a long of 0.2, a reduce-only sell of 0.5 closes 0.2 and stops at flat.
    h.market_order(a, BTC, Side::Buy, "0.2", "100000");
    let ev = h.cmd(reduce(Side::Sell, "0.5"));
    assert_eq!((fills(&ev)[0].qty, fills(&ev)[0].position), (f("0.2"), Fixed::ZERO));

    // In a reduce-only market, new risk is refused but closing still works.
    h.market_order(a, BTC, Side::Buy, "0.2", "100000");
    h.admin(AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true });
    assert_eq!(rejection(&h.market_order(a, BTC, Side::Buy, "0.1", "100000")), &Reject::ReduceOnly);
    assert_eq!(fills(&h.market_order(a, BTC, Side::Sell, "0.2", "100000")).len(), 1);
}

#[test]
fn triggered_orders_that_cannot_fill_are_cancelled() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100");
    h.price(BTC, "100000");
    h.order(a, BTC, Side::Buy, "1", OrderKind::Stop { price: f("101000") });
    let ev = h.price(BTC, "101000");
    assert!(ev.iter().any(|e| matches!(e, Event::OrderCancelled { reason: CancelReason::Unfillable, .. })));
}

#[test]
fn clients_can_cancel_their_resting_orders_once() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");
    let ev = h.order(a, BTC, Side::Buy, "0.1", OrderKind::Limit { price: f("90000") });
    let Event::OrderAccepted { order_id, .. } = ev[0] else { panic!("{ev:?}") };

    // Another account cannot cancel it.
    assert_eq!(
        rejection(&h.cmd(Command::CancelOrder { account: acct(2), order_id, auth: None })),
        &Reject::UnknownOrder
    );
    assert_eq!(h.engine.markets()[&BTC].resting.len(), 1);

    let ev = h.cmd(Command::CancelOrder { account: a, order_id, auth: None });
    assert_eq!(ev, vec![Event::OrderCancelled { account: a, order_id, reason: CancelReason::Client }]);
    assert!(h.engine.markets()[&BTC].resting.is_empty());
    // A second cancel finds nothing.
    assert_eq!(rejection(&h.cmd(Command::CancelOrder { account: a, order_id, auth: None })), &Reject::UnknownOrder);
}
