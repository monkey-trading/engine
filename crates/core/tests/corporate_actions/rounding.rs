use engine_core::Fixed;
use engine_core::account::Position;
use engine_core::command::{AdminCommand, OrderKind};
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::fixed::Rounding;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{OrderId, Side};

use crate::common::*;
use crate::support::*;

#[test]
fn inexact_splits_round_each_price_once_against_the_holder_or_toward_zero() {
    let mut h = held("1", "1");
    h.price(AAPL, "81");
    assert_fill(&h.market_order(acct(1), AAPL, Side::Buy, "2", "81"));
    assert_fill(&h.market_order(acct(2), AAPL, Side::Sell, "2", "81"));
    h.price(AAPL, "80.000000001");
    // Each resting price halves to a half nano-unit, rounded so the order never executes sooner.
    let resting = [
        (acct(1), Side::Buy, OrderKind::Limit { price: f("64.000000001") }, OrderKind::Limit { price: f("32") }),
        (
            acct(1),
            Side::Buy,
            OrderKind::Stop { price: f("100.000000001") },
            OrderKind::Stop { price: f("50.000000001") },
        ),
        (
            acct(2),
            Side::Sell,
            OrderKind::Limit { price: f("100.000000001") },
            OrderKind::Limit { price: f("50.000000001") },
        ),
        (acct(2), Side::Sell, OrderKind::Stop { price: f("64.000000001") }, OrderKind::Stop { price: f("32") }),
    ];
    for (account, side, kind, _) in resting {
        order_id(&h.order(account, AAPL, side, "1", kind));
    }
    let before = [acct(1), acct(2)].map(|account| h.engine.accounts()[&account].position(AAPL));
    let mark = h.engine.markets()[&AAPL].mark.unwrap();
    accepted(&schedule(&mut h, split("halves", 2, 1)));
    let events = tick(&mut h, open());
    assert!(events.iter().any(|e| matches!(e, Event::CorporateSplit { .. })), "{events:?}");
    let market = &h.engine.markets()[&AAPL];
    assert_eq!(market.mark, Some(f("40")), "80.000000001 halves toward zero");
    assert!(market.quotes.values().all(|quote| quote.mid == f("40")));
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).entry, f("40.333333334"), "a long's entry rounds up");
    for (account, original) in [acct(1), acct(2)].into_iter().zip(before) {
        let position = h.engine.accounts()[&account].position(AAPL);
        assert_eq!(position.qty, original.qty.mul_int(2));
        let against_holder = if original.qty.is_positive() { Rounding::Up } else { Rounding::Down };
        assert_eq!(position.entry, original.entry.checked_mul_div(Fixed::ONE, f("2"), against_holder).unwrap());
        let now = position.unrealised(f("40"), ProtocolVersion::V21);
        let then = original.unrealised(mark, ProtocolVersion::V21);
        assert!((now - then).abs() <= position.qty.abs().mul(f("0.000000002")), "{now} against {then}");
    }
    let orders: Vec<_> =
        [acct(1), acct(2)].iter().flat_map(|account| h.engine.accounts()[account].orders.values().cloned()).collect();
    assert_eq!(orders.len(), resting.len());
    for (order, (_, _, _, scaled)) in orders.iter().zip(resting) {
        assert_eq!((order.qty, order.kind), (f("2"), scaled));
    }
    assert_aggregates(&h);
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn rounded_off_position_fractions_settle_at_the_post_split_mark_and_dust_orders_cancel() {
    let mut h = held("2", "4");
    h.deposit(acct(3), "100");
    assert_fill(&h.market_order(acct(3), AAPL, Side::Buy, "0.000000002", "80"));
    let kept = order_id(&h.order(acct(1), AAPL, Side::Buy, "1", OrderKind::Limit { price: f("64") }));
    let dust = order_id(&h.order(acct(1), AAPL, Side::Buy, "0.000000001", OrderKind::Limit { price: f("64") }));
    h.price(AAPL, "80.5");
    let accounts = [acct(1), acct(2), acct(3)];
    let (balances, house) = (accounts.map(|account| h.balance(account)), h.engine.house_cash());
    accepted(&schedule(&mut h, split("reverse", 1, 3)));
    let events = tick(&mut h, open());
    // The mark is 241.5 and every entry 240, so each rounded-off fraction realises 1.5 per unit.
    // 2 / 3 leaves 2/3 of a nano-unit: 1.5 × 0.000000000667 = 0.000000001.
    // -4 / 3 leaves -1/3 of a nano-unit: -0.0000000005, rounded down to -0.000000001.
    // 0.000000002 / 3 rounds to nothing, closing the whole position for 0.000000001.
    let expected = [f("0.000000001"), f("-0.000000001"), f("0.000000001")];
    for ((account, before), amount) in accounts.into_iter().zip(balances).zip(expected) {
        let settled = events.iter().find_map(|e| match e {
            Event::CorporateCashInLieu { account: settled, amount, .. } if *settled == account => Some(*amount),
            _ => None,
        });
        assert_eq!(settled, Some(amount), "{events:?}");
        assert_eq!(h.balance(account), before + amount);
    }
    assert_eq!(h.engine.house_cash(), house - f("0.000000001"));
    let position = |account| h.engine.accounts()[&account].positions.get(&AAPL).copied();
    assert_eq!(position(acct(1)), Some(Position { qty: f("0.666666666"), entry: f("240") }));
    assert_eq!(position(acct(2)), Some(Position { qty: f("-1.333333333"), entry: f("240") }));
    assert_eq!(position(acct(3)), None);
    let orders = &h.engine.accounts()[&acct(1)].orders;
    assert_eq!(orders.keys().copied().collect::<Vec<_>>(), [kept]);
    assert_eq!((orders[&kept].qty, orders[&kept].kind), (f("0.333333333"), OrderKind::Limit { price: f("192") }));
    let cancelled = Event::OrderCancelled { account: acct(1), order_id: dust, reason: CancelReason::Split };
    assert!(events.contains(&cancelled), "{events:?}");
    assert!(!h.engine.markets()[&AAPL].resting.contains_key(&dust));
    assert_aggregates(&h);
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn cash_in_lieu_counts_as_realised_pnl_toward_emission_weight() {
    let mut h = held("2", "4");
    let emissions = EmissionSchedule { start_ms: h.ts, period_budget: TokenUnits(1_000_000) };
    h.operator_admin(AdminCommand::InitEmissions(emissions));
    h.price(AAPL, "80.5");
    accepted(&schedule(&mut h, split("reverse", 1, 3)));
    let events = tick(&mut h, open());
    assert!(events.iter().any(|e| matches!(e, Event::CorporateCashInLieu { .. })), "{events:?}");
    // The opening tick finalised every earlier period first, so only the split's settlements remain.
    let realised: Vec<_> =
        h.engine.period_results().iter().map(|(account, result)| (*account, result.realised)).collect();
    assert_eq!(realised, [(acct(1), f("0.000000001")), (acct(2), f("-0.000000001"))]);
}

#[test]
fn a_resting_order_whose_price_rounds_to_zero_is_cancelled_and_the_split_still_applies() {
    let mut h = held("2", "0");
    let cheap = order_id(&h.order(acct(1), AAPL, Side::Buy, "1", OrderKind::Limit { price: f("0.00000001") }));
    accepted(&schedule(&mut h, split("twenty", 20, 1)));
    let events = tick(&mut h, open());
    let cancelled = Event::OrderCancelled { account: acct(1), order_id: cheap, reason: CancelReason::Split };
    assert!(events.contains(&cancelled), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::CorporateActionApplied { .. })));
    assert!(h.engine.accounts()[&acct(1)].orders.is_empty());
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).qty, f("40"));
    assert!(!h.engine.markets()[&AAPL].corporate_action_blocked);
}

#[test]
fn a_resting_order_whose_scaled_price_overflows_is_cancelled_and_the_split_still_applies() {
    let mut h = held("2", "0");
    let far = order_id(&h.order(acct(1), AAPL, Side::Sell, "1", OrderKind::Limit { price: f("100") }));
    // A price that earlier extreme reverse splits could have reached; another 1-for-1,000,000,000
    // takes it past `i128`.
    let mut state = serde_json::to_value(&h.engine).unwrap();
    state["accounts"][acct(1).to_string()]["orders"][far.to_string()]["kind"]["price"] =
        serde_json::json!("1000000000000000000000000000");
    h.engine = serde_json::from_value(state).unwrap();
    accepted(&schedule(&mut h, split("reverse", 1, 1_000_000_000)));
    let events = tick(&mut h, open());
    let cancelled = Event::OrderCancelled { account: acct(1), order_id: far, reason: CancelReason::Split };
    assert!(events.contains(&cancelled), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::CorporateActionApplied { .. })), "{events:?}");
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).qty, f("0.000000002"));
}

#[test]
fn a_failed_split_blocks_dealing_and_financing_only_in_its_own_market() {
    let mut h = held("2", "0");
    let mut stock = h.engine.markets()[&AAPL].spec.clone();
    stock.financing.markup_bps = 3650;
    h.operator_admin(AdminCommand::UpsertMarket(stock));
    h.price(BTC, "100");
    assert_fill(&h.market_order(acct(1), BTC, Side::Buy, "1", "100"));
    // Scaled four-for-one, this price rounds to zero.
    h.price(AAPL, "0.000000003");
    accepted(&schedule(&mut h, split("zero-price", 4, 1)));
    // Entered per post-split share, this dividend must not pay on the blocked pre-split books.
    let mut later = dividend("later-cash", AAPL, "1", "0");
    later.previous_close = ny(2026, 10, 21, 16, 0);
    later.ex_open = ny(2026, 10, 22, 9, 30);
    accepted(&schedule(&mut h, later));
    let positions = h.engine.accounts()[&acct(1)].positions.clone();
    let events = tick(&mut h, open());
    assert!(
        events.iter().any(
            |e| matches!(e, Event::CorporateActionFailed { reason, .. } if reason.contains("price rounds to zero"))
        ),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, Event::CorporateSplit { .. })));
    assert_eq!(h.engine.accounts()[&acct(1)].positions, positions);
    let market = &h.engine.markets()[&AAPL];
    assert!(market.corporate_action_blocked && !market.open);
    let quoted = h.price(AAPL, "0.000000002");
    assert!(quoted.iter().any(|e| matches!(e, Event::PricesRejected { reason, .. } if reason.contains("blocked"))));
    let refused = h.market_order(acct(1), AAPL, Side::Buy, "1", "20");
    assert!(refused.iter().any(|e| matches!(e, Event::OrderRejected { reason: Reject::MarketClosed, .. })));
    let mut another = dividend("after-failure", AAPL, "1", "0");
    another.previous_close = ny(2026, 10, 22, 16, 0);
    another.ex_open = ny(2026, 10, 23, 9, 30);
    rejected(&schedule(&mut h, another), "blocked");
    let events = tick(&mut h, open() + 86_400_000);
    assert!(
        events.iter().any(|e| matches!(e, Event::CorporateActionFailed { id, reason, .. }
            if id == "later-cash" && reason.contains("blocked"))),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, Event::CorporateDividend { .. })));
    assert!(!h.engine.corporate_actions().contains_key("later-cash"));
    let financed: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Financing { market, .. } => Some(*market),
            _ => None,
        })
        .collect();
    assert!(financed.contains(&BTC) && !financed.contains(&AAPL), "{financed:?}");
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn an_account_a_split_leaves_flat_writes_off_its_bad_debt() {
    let mut h = held("0", "0");
    h.deposit(acct(3), "11");
    assert_fill(&h.market_order(acct(3), AAPL, Side::Buy, "0.000000002", "80"));
    h.price(BTC, "100");
    assert_fill(&h.market_order(acct(3), BTC, Side::Buy, "1", "100"));
    accepted(&schedule(&mut h, split("reverse", 1, 3)));
    // With the stock closed, a crypto gap closes the coin at a loss beyond the account's cash; the
    // write-off waits while the stock dust remains.
    h.advance_to(close() + 60_000);
    let gap = h.price(BTC, "50");
    assert!(gap.iter().any(|e| matches!(e, Event::CloseOut { account, .. } if *account == acct(3))), "{gap:?}");
    let debt = -h.balance(acct(3));
    assert!(debt.is_positive() && h.engine.accounts()[&acct(3)].positions.contains_key(&AAPL));
    let events = tick(&mut h, open());
    assert!(events.contains(&Event::BadDebt { account: acct(3), amount: debt }), "{events:?}");
    assert!(h.engine.accounts()[&acct(3)].positions.is_empty());
    assert_eq!(h.balance(acct(3)), Fixed::ZERO);
    h.assert_conserved(Fixed::ZERO);
}

/// The id of the order an `OrderAccepted` event names.
fn order_id(events: &[Event]) -> OrderId {
    events
        .iter()
        .find_map(|e| match e {
            Event::OrderAccepted { order_id, .. } => Some(*order_id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("order rests: {events:?}"))
}

/// The stock's aggregates equal the sums over the positions held in it.
fn assert_aggregates(h: &Harness) {
    let market = &h.engine.markets()[&AAPL];
    let positions: Vec<Position> =
        h.engine.accounts().values().filter_map(|a| a.positions.get(&AAPL).copied()).collect();
    assert_eq!(market.client_qty, positions.iter().map(|p| p.qty).sum::<Fixed>());
    assert_eq!(market.client_cost, positions.iter().map(Position::cost).sum::<Fixed>());
}
