use engine_core::Fixed;
use engine_core::command::{Command, OrderKind};
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::*;
use crate::support::*;

#[test]
fn four_for_one_preserves_value_pnl_cash_aggregates_limits_markouts_and_stress_references() {
    let mut h = held("2", "3");
    // A day-long move window keeps the pre-split buckets through the opening's stress refresh.
    let mut params = h.engine.params().unwrap().clone();
    params.stress_move_window_ms = 86_400_000;
    h.operator_admin(engine_core::command::AdminCommand::SetParams(params));
    let mut spec = h.engine.markets()[&AAPL].spec.clone();
    spec.schedule = serde_json::from_value(serde_json::json!({
        "kind": "weekly", "tz": "UTC",
        "windows": [{"open":"Tue 14:00", "close":"Tue 14:01"}],
        "special_sessions": [{"from":"2026-10-20T14:02:00", "to":"2026-10-20T15:00:00"}]
    }))
    .unwrap();
    h.operator_admin(engine_core::command::AdminCommand::UpsertMarket(spec));
    h.price(AAPL, "96");
    let before = h.engine.markets()[&AAPL].clone();
    let long = h.engine.accounts()[&acct(1)].position(AAPL);
    let short = h.engine.accounts()[&acct(2)].position(AAPL);
    let money = (h.balance(acct(1)), h.balance(acct(2)), h.engine.house_cash());
    let markouts = serde_json::to_value(&h.engine).unwrap()["markouts"].clone();
    let mut action = split("four", 4, 1);
    action.previous_close = h.ts + 60_000;
    action.ex_open = h.ts + 120_000;
    let application = action.ex_open;
    accepted(&schedule(&mut h, action));
    // A market not refreshed since V13 still carries its legacy move reference.
    let legacy_at = h.ts - 1_000;
    let mut state = serde_json::to_value(&h.engine).unwrap();
    state["markets"][AAPL.to_string()]["stress"]["move_ref"] = serde_json::json!([legacy_at, "96"]);
    h.engine = serde_json::from_value(state).unwrap();
    let windows = h.engine.markets()[&AAPL].stress.move_window.clone();
    assert!(!windows.is_empty());
    h.engine.state_root();
    let events = tick(&mut h, application);
    assert!(events.iter().any(|e| matches!(e, Event::CorporateSplit { new_units: 4, old_units: 1, .. })), "{events:?}");
    let mut rebuilt = restored(&h.engine);
    assert_eq!(h.engine.state_root(), rebuilt.state_root(), "every scaled leaf, markouts included, is recorded");
    let market = &h.engine.markets()[&AAPL];
    for bucket in &windows {
        let scaled = market.stress.move_window.iter().find(|b| b.from == bucket.from).expect("bucket kept");
        assert_eq!((scaled.low, scaled.high), (bucket.low.div_int(4), bucket.high.div_int(4)));
    }
    // The opening's refresh moves the scaled legacy reference into the window.
    let legacy =
        market.stress.move_ref.map(|(at, mid)| (at, mid, mid)).or_else(|| {
            market.stress.move_window.iter().find(|b| b.from == legacy_at).map(|b| (b.from, b.low, b.high))
        });
    assert_eq!(legacy, Some((legacy_at, f("24"), f("24"))));
    assert_eq!(market.mark, Some(f("24")));
    // The split's brief closure drops the pre-close quotes at the opening, so no scaled quote or
    // processed index survives; the marks, positions and stress leaves above record the scaling.
    assert_eq!(market.processed_index, None);
    assert!(market.quotes.is_empty());
    assert_eq!(market.client_qty, before.client_qty.mul_int(4));
    assert_eq!(market.client_cost, before.client_cost);
    assert_eq!(market.spec, before.spec, "USD bounds and notional tiers do not scale as shares");
    assert_eq!(money, (h.balance(acct(1)), h.balance(acct(2)), h.engine.house_cash()));
    for (account, original) in [(acct(1), long), (acct(2), short)] {
        let position = h.engine.accounts()[&account].position(AAPL);
        assert_eq!(position.qty, original.qty.mul_int(4));
        assert_eq!(position.entry, original.entry.div_int(4));
        assert_eq!(position.qty.mul(market.mark.unwrap()), original.qty.mul(before.mark.unwrap()));
        assert_eq!(
            position.unrealised(market.mark.unwrap(), ProtocolVersion::V23),
            original.unrealised(before.mark.unwrap(), ProtocolVersion::V23)
        );
    }
    let now = serde_json::to_value(&h.engine).unwrap();
    assert!(!markouts.as_array().unwrap().is_empty());
    assert_eq!(markouts.as_array().unwrap().len(), now["markouts"].as_array().unwrap().len());
    for (old, adjusted) in markouts.as_array().unwrap().iter().zip(now["markouts"].as_array().unwrap()) {
        assert_eq!(old["notional"], adjusted["notional"]);
        let price: Fixed = serde_json::from_value(old["price"].clone()).unwrap();
        assert_eq!(serde_json::from_value::<Fixed>(adjusted["price"].clone()).unwrap(), price.div_int(4));
    }
    h.assert_conserved(Fixed::ZERO);
    h.price(AAPL, "24");
    assert_fill(&h.market_order(acct(1), AAPL, Side::Buy, "1", "24"));
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).qty, f("9"));
}

#[test]
fn split_normalizes_limit_stop_and_hot_trigger_indexes_before_an_opening_quote() {
    let mut h = held("2", "0");
    h.order(acct(1), AAPL, Side::Buy, "1", OrderKind::Limit { price: f("64") });
    h.order(acct(1), AAPL, Side::Buy, "1", OrderKind::Stop { price: f("128") });
    accepted(&schedule(&mut h, split("orders", 4, 1)));
    h.advance_to(open());
    let events = h.price(AAPL, "20");
    assert!(
        !events.iter().any(|e| matches!(e, Event::Fill(_))),
        "opening price must see adjusted triggers: {events:?}"
    );
    let orders: Vec<_> = h.engine.accounts()[&acct(1)].orders.values().collect();
    assert_eq!(orders.len(), 2);
    assert_eq!((orders[0].qty, orders[0].kind), (f("4"), OrderKind::Limit { price: f("16") }));
    assert_eq!((orders[1].qty, orders[1].kind), (f("4"), OrderKind::Stop { price: f("32") }));
    assert_fill(&h.price(AAPL, "16"));
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).qty, f("12"));
    assert_eq!(h.engine.accounts()[&acct(1)].orders.len(), 1);
    assert_fill(&h.price(AAPL, "32"));
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).qty, f("16"));
    assert!(h.engine.accounts()[&acct(1)].orders.is_empty());
    let mut rebuilt = restored(&h.engine);
    assert_eq!(h.engine.state_root(), rebuilt.state_root(), "all position/order/markout leaves record split changes");
}

#[test]
fn old_timestamped_quotes_are_rejected_and_no_new_quote_order_uses_adjusted_units() {
    let mut h = held("2", "0");
    let pre_split = h.ts;
    accepted(&schedule(&mut h, split("quotes", 4, 1)));
    tick(&mut h, open());
    let events = h.cmd(Command::Prices { market: AAPL, quotes: vec![quote("late-old", "80", pre_split)] });
    assert!(events.iter().any(|e| matches!(e, Event::PricesRejected { reason, .. } if reason.contains("predates"))));
    assert!(!h.engine.markets()[&AAPL].quotes.contains_key("late-old"));
    h.price(AAPL, "20");
    let events = h.market_order(acct(1), AAPL, Side::Buy, "1", "20");
    let fill = events
        .iter()
        .find_map(|e| match e {
            Event::Fill(fill) => Some(fill),
            _ => None,
        })
        .unwrap();
    assert_eq!(fill.price, f("20"));
    assert_eq!(fill.qty, f("1"));
}

#[test]
fn incoming_opening_gap_is_stored_after_normalization_before_any_resting_fill() {
    let mut h = held("2", "0");
    accepted(&schedule(&mut h, split("opening-gap", 4, 1)));
    tick(&mut h, close());
    h.order(acct(1), AAPL, Side::Buy, "1", OrderKind::Limit { price: f("120") });
    h.advance_to(open());
    let events = h.price(AAPL, "40");
    assert!(
        !events.iter().any(|e| matches!(e, Event::Fill(_))),
        "the normalized prior quote must not fill before this command's opening gap: {events:?}"
    );
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).qty, f("8"));
    assert_eq!(h.engine.markets()[&AAPL].mark, Some(f("40")));
    let order = h.engine.accounts()[&acct(1)].orders.values().next().unwrap();
    assert_eq!((order.qty, order.kind), (f("4"), OrderKind::Limit { price: f("30") }));
}

#[test]
fn split_preserves_quote_age_so_an_order_without_a_fresh_quote_gets_no_price() {
    let mut h = held("2", "0");
    let quoted_at = h.ts;
    let mut spec = h.engine.markets()[&AAPL].spec.clone();
    spec.price_rules.max_age_ms = 1000;
    h.operator_admin(engine_core::command::AdminCommand::UpsertMarket(spec));
    accepted(&schedule(&mut h, split("stale", 4, 1)));
    tick(&mut h, open());
    assert_eq!(h.engine.markets()[&AAPL].mark, Some(f("20")));
    assert!(h.engine.markets()[&AAPL].quotes.values().all(|q| q.mid == f("20") && q.ts == quoted_at));
    let events = h.market_order(acct(1), AAPL, Side::Buy, "1", "20");
    assert!(
        events.iter().any(|e| matches!(e, Event::OrderRejected { reason: Reject::NoPrice { .. }, .. })),
        "{events:?}"
    );
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).qty, f("8"));
}

#[test]
fn quotes_during_the_pending_split_closure_cannot_be_scaled_twice_at_open() {
    let mut h = held("2", "0");
    accepted(&schedule(&mut h, split("premarket", 4, 1)));
    h.advance_to(close() + 1);
    let events = h.price(AAPL, "20");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::PricesRejected { reason, .. } if reason.contains("pending split closure"))),
        "{events:?}"
    );
    // The pre-close quotes drop at this first command after the closure, and the refused quote
    // stores nothing.
    assert!(h.engine.markets()[&AAPL].quotes.is_empty());
    tick(&mut h, open());
    h.price(AAPL, "20");
    let events = h.market_order(acct(1), AAPL, Side::Buy, "1", "20");
    assert!(events.iter().any(|e| matches!(e, Event::Fill(fill) if fill.price == f("20"))), "{events:?}");
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).qty, f("9"));
}

#[test]
fn a_failed_split_refuses_the_opening_quote_and_commits_nothing() {
    let mut h = held("2", "0");
    // Scaled four-for-one, this price rounds to zero, so the split fails.
    h.price(AAPL, "0.000000003");
    accepted(&schedule(&mut h, split("zero-price-opening", 4, 1)));
    let position = h.engine.accounts()[&acct(1)].position(AAPL);
    h.advance_to(open());
    let events = h.price(AAPL, "20");
    assert!(events.iter().any(|e| matches!(e, Event::CorporateActionFailed { .. })), "{events:?}");
    assert!(
        events.iter().any(|e| matches!(e, Event::PricesRejected { reason, .. } if reason.contains("blocked"))),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, Event::Fill(_))));
    // The refused opening quote stores nothing; the pre-close quotes dropped across the closure
    // at this first command after it, and the failed split commits nothing.
    assert!(h.engine.markets()[&AAPL].quotes.is_empty());
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL), position);
}

#[test]
fn a_split_failing_on_a_later_account_commits_nothing_for_earlier_ones_or_the_market() {
    // The first holder's 0.1 shares scale within bounds; the second holder's quantity overflows
    // `i128` times `u32::MAX`, after the first holder's books were preflighted.
    let mut h = held("0.1", "0");
    accepted(&schedule(&mut h, split("overflow", u32::MAX, 1)));
    let mut state = serde_json::to_value(&h.engine).unwrap();
    let huge = Fixed::from_raw(10_i128.pow(29));
    let market = &mut state["markets"][AAPL.to_string()];
    let aggregate = |value: &serde_json::Value| serde_json::from_value::<Fixed>(value.clone()).unwrap();
    market["client_qty"] = serde_json::json!(aggregate(&market["client_qty"]) + huge);
    market["client_cost"] = serde_json::json!(aggregate(&market["client_cost"]) + huge.mul(f("0.000000001")));
    // Market prices large enough to stay positive at the ratio, so only the quantity overflows.
    market["mark"] = serde_json::json!("5");
    market["processed_index"]["mid"] = serde_json::json!("5");
    for quote in market["quotes"].as_object_mut().unwrap().values_mut() {
        quote["mid"] = serde_json::json!("5");
    }
    state["accounts"][acct(2).to_string()]["positions"][AAPL.to_string()] =
        serde_json::json!({ "qty": huge, "entry": "0.000000001" });
    h.engine = serde_json::from_value(state).unwrap();
    let books = |h: &Harness, account| {
        let state = &h.engine.accounts()[&account];
        (state.balance, state.positions.clone(), state.orders.clone())
    };
    let (first, second) = (books(&h, acct(1)), books(&h, acct(2)));
    let (market, house) = (h.engine.markets()[&AAPL].clone(), h.engine.house_cash());
    let events = tick(&mut h, open());
    assert!(
        events.iter().any(|e| matches!(e, Event::CorporateActionFailed { reason, .. } if reason.contains("overflow"))),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, Event::CorporateSplit { .. })));
    assert_eq!((books(&h, acct(1)), books(&h, acct(2))), (first, second));
    let after = &h.engine.markets()[&AAPL];
    assert_eq!(
        (after.mark, after.client_qty, after.client_cost, after.last_split_at),
        (market.mark, market.client_qty, market.client_cost, market.last_split_at)
    );
    // The failed split commits nothing, so it cannot scale the pre-close quotes; V21 drops them
    // across the closure at this opening command.
    assert!(after.quotes.is_empty());
    assert_eq!(serde_json::to_value(&after.stress).unwrap(), serde_json::to_value(&market.stress).unwrap());
    assert_eq!(h.engine.house_cash(), house);
}

#[test]
fn split_quantities_beyond_the_input_bound_fail_a_position_and_cancel_a_resting_order() {
    for resting in [false, true] {
        let mut h = fixture();
        let mut params = h.engine.params().unwrap().clone();
        params.house_total_cap_floor = Fixed::MAX_INPUT;
        h.operator_admin(engine_core::command::AdminCommand::SetParams(params));
        let mut spec = h.engine.markets()[&AAPL].spec.clone();
        spec.house_cap_floor = Fixed::MAX_INPUT;
        for tier in &mut spec.spread_tiers {
            tier.bps = 0;
        }
        h.operator_admin(engine_core::command::AdminCommand::UpsertMarket(spec));
        h.deposit(acct(1), "100000000");
        h.price(AAPL, "0.04");
        // Every scaled price stays exact, so only the quantity bound acts.
        let order = resting.then(|| {
            let events = h.order(acct(1), AAPL, Side::Buy, "1000000000", OrderKind::Limit { price: f("0.02") });
            events
                .iter()
                .find_map(|e| match e {
                    Event::OrderAccepted { order_id, .. } => Some(*order_id),
                    _ => None,
                })
                .expect("order rests")
        });
        if !resting {
            assert_fill(&h.market_order(acct(1), AAPL, Side::Buy, "300000000", "0.04"));
        }
        accepted(&schedule(&mut h, split("beyond-bound", 4, 1)));
        let account = h.engine.accounts()[&acct(1)].clone();
        let events = tick(&mut h, open());
        let after = &h.engine.accounts()[&acct(1)];
        if let Some(order_id) = order {
            // A client-sized order cannot block the split: it is cancelled and the split applies.
            let cancelled = Event::OrderCancelled { account: acct(1), order_id, reason: CancelReason::Split };
            assert!(events.contains(&cancelled), "{events:?}");
            assert!(events.iter().any(|e| matches!(e, Event::CorporateActionApplied { .. })));
            assert!(after.orders.is_empty());
        } else {
            assert!(
                events.iter().any(
                    |e| matches!(e, Event::CorporateActionFailed { reason, .. } if reason.contains("input bound"))
                ),
                "{events:?}"
            );
            assert_eq!(after.positions, account.positions);
        }
        h.assert_conserved(Fixed::ZERO);
    }
}

#[test]
fn same_open_dividends_pay_pre_split_holdings_and_later_dividends_pay_post_split_holdings() {
    let mut h = held("2", "3");
    // The split's id sorts first, so only the adjustment kind can order the two.
    accepted(&schedule(&mut h, split("a-split", 4, 1)));
    accepted(&schedule(&mut h, dividend("b-cash", AAPL, "4", "0.25")));
    let mut later = dividend("later", AAPL, "8", "0");
    later.previous_close = ny(2026, 10, 21, 16, 0);
    later.ex_open = ny(2026, 10, 22, 9, 30);
    accepted(&schedule(&mut h, later.clone()));
    let events = tick(&mut h, open());
    assert_eq!(payment(&events, 1), f("6"));
    assert_eq!(payment(&events, 2), f("-12"));
    let split_at = events.iter().position(|e| matches!(e, Event::CorporateSplit { .. })).unwrap();
    let cash_at = events.iter().position(|e| matches!(e, Event::CorporateDividend { .. })).unwrap();
    assert!(cash_at < split_at, "same-opening dividends pay the captured pre-split holdings first");
    assert_eq!(h.engine.corporate_actions()["later"].instruction, later, "a split leaves pending amounts as signed");
    let events = tick(&mut h, ny(2026, 10, 22, 9, 30));
    assert_eq!(payment(&events, 1), f("64"), "eight post-split shares at the signed 8");
    assert_eq!(payment(&events, 2), f("-96"));
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn foreign_currency_split_preserves_fx_marks_and_usd_value_and_pnl() {
    let mut h = fixture();
    h.list_fx();
    let mut stock = h.engine.markets()[&AAPL].spec.clone();
    stock.quote_currency = engine_core::market::QuoteCurrency::Eur { fx_market: EURUSD };
    h.operator_admin(engine_core::command::AdminCommand::UpsertMarket(stock));
    h.price(EURUSD, "1.25");
    h.deposit(acct(1), "10000");
    h.price(AAPL, "80");
    assert_fill(&h.market_order(acct(1), AAPL, Side::Buy, "2", "80"));
    h.price(AAPL, "96");
    let before = h.engine.markets()[&AAPL].clone();
    let old = h.engine.accounts()[&acct(1)].position(AAPL);
    let equity = h.equity_and_margin(acct(1)).0;
    let fx_quotes = h.engine.markets()[&EURUSD].quotes.clone();
    accepted(&schedule(&mut h, split("foreign-stock", 4, 1)));
    let events = tick(&mut h, open());
    assert!(events.iter().any(|e| matches!(e, Event::CorporateSplit { .. })), "{events:?}");
    let after = &h.engine.markets()[&AAPL];
    let position = h.engine.accounts()[&acct(1)].position(AAPL);
    assert_eq!(after.fx_mark, Some(f("1.25")));
    assert_eq!(h.engine.markets()[&EURUSD].quotes, fx_quotes);
    assert_eq!(
        after.usd_at_mark(position.qty.mul(after.mark.unwrap()), engine_core::fixed::Rounding::Down),
        before.usd_at_mark(old.qty.mul(before.mark.unwrap()), engine_core::fixed::Rounding::Down)
    );
    assert_eq!(h.equity_and_margin(acct(1)).0, equity);
    assert_eq!(
        position.unrealised(after.mark.unwrap(), ProtocolVersion::V23),
        old.unrealised(before.mark.unwrap(), ProtocolVersion::V23)
    );
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn reverse_split_preserves_internal_prices_above_the_external_input_bound() {
    let mut h = fixture();
    let mut params = h.engine.params().unwrap().clone();
    params.house_total_cap_floor = Fixed::MAX_INPUT;
    h.operator_admin(engine_core::command::AdminCommand::SetParams(params));
    let mut spec = h.engine.markets()[&AAPL].spec.clone();
    spec.house_cap_floor = Fixed::MAX_INPUT;
    for tier in &mut spec.spread_tiers {
        tier.bps = 0;
    }
    h.operator_admin(engine_core::command::AdminCommand::UpsertMarket(spec));
    h.deposit(acct(1), "1000000000");
    h.price(AAPL, "1000000000");
    assert_fill(&h.market_order(acct(1), AAPL, Side::Buy, "1", "1000000000"));
    h.price(AAPL, "100000000");
    let before = h.engine.accounts()[&acct(1)].position(AAPL);
    let equity = h.equity_and_margin(acct(1));
    accepted(&schedule(&mut h, split("reverse-exact", 1, 4)));
    let events = tick(&mut h, open());
    assert!(events.iter().any(|e| matches!(e, Event::CorporateSplit { .. })), "{events:?}");
    let position = h.engine.accounts()[&acct(1)].position(AAPL);
    assert_eq!((position.qty, position.entry), (f("0.25"), f("4000000000")));
    assert_eq!(h.engine.markets()[&AAPL].mark, Some(f("400000000")));
    assert_eq!(
        position.unrealised(f("400000000"), ProtocolVersion::V23),
        before.unrealised(f("100000000"), ProtocolVersion::V23)
    );
    assert_eq!(h.equity_and_margin(acct(1)), equity);
    h.price(AAPL, "400000000");
    let events = h.market_order(acct(1), AAPL, Side::Sell, "0.25", "400000000");
    assert_fill(&events);
    assert!(h.engine.accounts()[&acct(1)].position(AAPL).qty.is_zero());
    assert_eq!(h.balance(acct(1)), f("100000000"));
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn reverse_split_with_quotes_above_the_feed_input_bound_produces_no_price() {
    let mut h = fixture();
    h.deposit(acct(1), "10000");
    h.price(AAPL, "1000000000");
    accepted(&schedule(&mut h, split("above-feed-bound", 1, 4)));
    let events = tick(&mut h, open());
    assert!(events.iter().any(|e| matches!(e, Event::CorporateSplit { .. })), "{events:?}");
    let market = &h.engine.markets()[&AAPL];
    assert!(!market.corporate_action_blocked);
    assert_eq!(market.mark, Some(f("4000000000")));
    assert!(market.quotes.values().all(|q| q.mid == f("4000000000")));
    assert!(market.processed_index.is_none());
    let events = h.market_order(acct(1), AAPL, Side::Buy, "1", "1000000000");
    assert!(
        events.iter().any(|e| matches!(e, Event::OrderRejected { reason: Reject::NoPrice { .. }, .. })),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, Event::Fill(_))));
}
