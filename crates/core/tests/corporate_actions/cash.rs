use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::market::QuoteCurrency;
use engine_core::session::Schedule;
use engine_core::types::Side;

use crate::common::*;
use crate::support::*;

#[test]
fn long_net_short_gross_and_house_offset_conserve_cash_at_the_ex_date_open() {
    let mut h = held("2", "3");
    accepted(&schedule(&mut h, dividend("cash", AAPL, "4", "0.25")));
    let before = (h.balance(acct(1)), h.balance(acct(2)), h.engine.house_cash());
    let captured = tick(&mut h, close());
    assert!(
        captured
            .iter()
            .any(|e| matches!(e, Event::CorporateEntitlementsCaptured { accounts: 2, at, .. } if *at == close()))
    );
    assert_eq!(h.balance(acct(1)), before.0);
    assert_eq!(h.balance(acct(2)), before.1);
    let events = tick(&mut h, open());
    assert_eq!(payment(&events, 1), f("6"));
    assert_eq!(payment(&events, 2), f("-12"));
    assert_eq!(h.balance(acct(1)), before.0 + f("6"));
    assert_eq!(h.balance(acct(2)), before.1 - f("12"));
    assert_eq!(h.engine.house_cash(), before.2 + f("6"));
    h.assert_conserved(Fixed::ZERO);
    assert!(h.engine.corporate_actions().is_empty());
    assert!(!h.cmd(Command::Tick).iter().any(|e| matches!(e, Event::CorporateDividend { .. })));
    rejected(&schedule(&mut h, dividend("cash", AAPL, "4", "0.25")), "cutoff");
}

#[test]
fn withholding_boundaries_and_subnano_amounts_round_toward_the_house() {
    for (rate, net) in [("0", "0"), ("1", "0"), ("0.5", "0")] {
        let mut h = held("0.5", "0.5");
        accepted(&schedule(&mut h, dividend("tiny", AAPL, "0.000000001", rate)));
        let before = h.balance(acct(2));
        let events = tick(&mut h, open());
        assert_eq!(payment(&events, 1), f(net), "rate={rate}");
        assert_eq!(payment(&events, 2), f("-0.000000001"), "rate={rate}");
        assert_eq!(h.balance(acct(2)), before - f("0.000000001"));
        h.assert_conserved(Fixed::ZERO);
    }
    for (rate, expected) in [("0", "2"), ("1", "0")] {
        let mut h = held("2", "2");
        accepted(&schedule(&mut h, dividend("boundary", AAPL, "1", rate)));
        let events = tick(&mut h, open());
        assert_eq!(payment(&events, 1), f(expected));
        assert_eq!(payment(&events, 2), f("-2"));
    }
}

#[test]
fn index_dividend_points_use_recorded_fx_marks_with_signed_rounding() {
    for currency in [
        QuoteCurrency::Eur { fx_market: EURUSD },
        QuoteCurrency::Gbp { fx_market: EURUSD },
        QuoteCurrency::Jpy { fx_market: USDJPY },
    ] {
        let mut h = fixture();
        h.list_fx();
        if matches!(currency, QuoteCurrency::Gbp { .. }) {
            let mut fx = h.engine.markets()[&EURUSD].spec.clone();
            // No positions or pending actions exist yet, so the index listing can be replaced too.
            let mut de = h.engine.markets()[&DE40].spec.clone();
            de.quote_currency = QuoteCurrency::Usd;
            h.operator_admin(AdminCommand::UpsertMarket(de));
            fx.symbol = "GBP-USD".into();
            h.operator_admin(AdminCommand::UpsertMarket(fx));
        }
        let mut spec = h.engine.markets()[&DE40].spec.clone();
        spec.schedule = us_equities();
        spec.quote_currency = currency;
        spec.financing.benchmark = "SOFR".into();
        spec.financing.markup_bps = 0;
        h.operator_admin(AdminCommand::UpsertMarket(spec));
        h.price(EURUSD, "1.25");
        h.price(USDJPY, "150");
        h.price(DE40, "80");
        h.deposit(acct(1), "10000");
        h.deposit(acct(2), "10000");
        assert_fill(&h.market_order(acct(1), DE40, Side::Buy, "1", "80"));
        assert_fill(&h.market_order(acct(2), DE40, Side::Sell, "1", "80"));
        accepted(&schedule(&mut h, dividend("index", DE40, "1", "0.2")));
        let events = tick(&mut h, open());
        let (long, short) = if matches!(currency, QuoteCurrency::Jpy { .. }) {
            ("0.005333333", "-0.006666667")
        } else {
            ("1", "-1.25")
        };
        assert_eq!(payment(&events, 1), f(long));
        assert_eq!(payment(&events, 2), f(short));
        assert!(
            h.engine.live_fx(DE40).is_err(),
            "the day-old FX quote is stale; settlement uses its recorded valuation mark"
        );
        h.assert_conserved(Fixed::ZERO);
    }
}

#[test]
fn absent_fx_fails_the_dividend_and_leaves_its_market_dealing() {
    let mut h = held("2", "1");
    h.list_fx();
    // A dividend can be scheduled before the market has ever had an FX mark.
    let mut spec = h.engine.markets()[&DE40].spec.clone();
    spec.schedule = us_equities();
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    accepted(&schedule(&mut h, dividend("no-fx", DE40, "1", "0")));
    let before = (h.balance(acct(1)), h.balance(acct(2)), h.engine.house_cash());
    let events = tick(&mut h, open());
    assert!(
        events.iter().any(|e| matches!(e, Event::CorporateActionFailed { reason, .. } if reason.contains("FX"))),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, Event::CorporateDividend { .. })));
    assert_eq!((h.balance(acct(1)), h.balance(acct(2)), h.engine.house_cash()), before);
    // A failed dividend pays nothing but leaves dealing, quotes and the listing open.
    let market = &h.engine.markets()[&DE40];
    assert!(!market.corporate_action_blocked && market.open);
    assert!(!h.engine.corporate_actions().contains_key("no-fx"));
    assert!(!h.price(DE40, "81").iter().any(|e| matches!(e, Event::PricesRejected { .. })));
    let mut spec = h.engine.markets()[&DE40].spec.clone();
    spec.schedule = Schedule::AlwaysOpen;
    h.operator_admin(AdminCommand::UpsertMarket(spec));
}

#[test]
fn a_dividend_failing_on_a_later_account_credits_no_earlier_one() {
    let mut h = held("2", "0");
    h.deposit(acct(3), "10000");
    assert_fill(&h.market_order(acct(3), AAPL, Side::Buy, "3", "80"));
    accepted(&schedule(&mut h, dividend("overflowing", AAPL, "1000000000", "0")));
    // The third account's balance cannot take its 3,000,000,000 credit; the first account's
    // 2,000,000,000 credit is computed before it.
    let mut state = serde_json::to_value(&h.engine).unwrap();
    state["accounts"][acct(3).to_string()]["balance"] = serde_json::json!(Fixed::from_raw(i128::MAX - 10_i128.pow(18)));
    h.engine = serde_json::from_value(state).unwrap();
    let before = (h.balance(acct(1)), h.balance(acct(3)), h.engine.house_cash());
    let events = tick(&mut h, open());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::CorporateActionFailed { reason, .. } if reason.contains("balance overflows"))),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, Event::CorporateDividend { .. })));
    assert_eq!((h.balance(acct(1)), h.balance(acct(3)), h.engine.house_cash()), before);
    assert!(!h.engine.markets()[&AAPL].corporate_action_blocked);
}

#[test]
fn dividend_debit_checks_margin_and_closes_losing_tradeable_positions_immediately() {
    let mut h = held("0", "10");
    // A late pre-close deposit is irrelevant to eligibility; the short debit itself takes the
    // account below close-out even though no quote arrives at application.
    let action = dividend("margin", AAPL, "1000", "0");
    accepted(&schedule(&mut h, action));
    h.price(AAPL, "84");
    let events = tick(&mut h, open());
    assert_eq!(payment(&events, 2), f("-10000"));
    let adjustment = events.iter().position(|e| matches!(e, Event::CorporateDividend { .. })).unwrap();
    let closeout = events
        .iter()
        .position(|e| matches!(e, Event::CloseOut { account, .. } if *account == acct(2)))
        .expect("debit triggers closeout");
    assert!(adjustment < closeout);
    assert!(h.engine.accounts()[&acct(2)].position(AAPL).qty.is_zero());
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn dividend_debit_checks_every_account_when_the_opening_has_no_fresh_quote() {
    // At the ex-date open and one minute later, past the open: neither market reprocesses a price,
    // so only the post-adjustment margin check can close the debited account.
    for at in [open(), open() + 60_000] {
        let mut h = held("0", "10");
        let mut stock = h.engine.markets()[&AAPL].spec.clone();
        stock.price_rules.max_age_ms = 600_000;
        h.operator_admin(AdminCommand::UpsertMarket(stock));
        let mut coin = h.engine.markets()[&BTC].spec.clone();
        coin.price_rules.max_age_ms = 3_600_000;
        h.operator_admin(AdminCommand::UpsertMarket(coin));
        h.price(BTC, "100");
        assert_fill(&h.market_order(acct(2), BTC, Side::Buy, "1", "100"));
        accepted(&schedule(&mut h, dividend("stale-open", AAPL, "1000", "0")));
        h.advance_to(open() - 500);
        h.price(BTC, "100");
        let events = tick(&mut h, at);
        assert_eq!(payment(&events, 2), f("-10000"), "at={at}");
        assert!(h.engine.markets()[&AAPL].processed_index.is_none(), "the stock's last quote is a day old");
        assert!(
            events.iter().any(|e| matches!(e, Event::CloseOut { account, .. } if *account == acct(2))),
            "at={at}: {events:?}"
        );
        assert!(h.engine.accounts()[&acct(2)].position(BTC).qty.is_zero());
        h.assert_conserved(Fixed::ZERO);
    }
}

#[test]
fn dividend_margin_check_values_the_opening_quote_in_the_same_command() {
    let mut h = held("0", "10");
    h.price(AAPL, "81");
    let withdrawal = h.cmd(Command::Withdraw { account: acct(2), amount: f("9828"), auth: None });
    assert!(withdrawal.iter().any(|e| matches!(e, Event::WithdrawalApproved { .. })), "{withdrawal:?}");
    accepted(&schedule(&mut h, dividend("opening-mark", AAPL, "10", "0")));
    tick(&mut h, close());
    h.advance_to(open());
    let events = h.price(AAPL, "71");
    assert_eq!(payment(&events, 2), f("-100"));
    assert!(
        !events.iter().any(|e| matches!(e, Event::CloseOut { .. })),
        "cash debit and actual opening mark must be valued together: {events:?}"
    );
    assert_eq!(h.engine.accounts()[&acct(2)].position(AAPL).qty, f("-10"));
    assert_eq!(h.balance(acct(2)), f("72"));
    h.assert_conserved(f("9828"));
}
