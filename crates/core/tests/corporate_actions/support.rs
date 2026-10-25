#![allow(dead_code)]

use engine_core::command::{AdminCommand, Command};
use engine_core::corporate::{Adjustment, CorporateAction};
use engine_core::event::Event;
use engine_core::types::{MarketId, Millis, Side};
use engine_core::{Engine, Fixed};

use crate::common::*;

pub fn fixture() -> Harness {
    let mut h = Harness::v17();
    h.admin(AdminCommand::ActivateProtocol { version: engine_core::protocol::ProtocolVersion::V18 });
    // Source-only V21 development precedes predecessor integration and activation. A serialized
    // test state exercises the new rules without publishing an activation or reference fixture.
    let mut state = serde_json::to_value(&h.engine).unwrap();
    state["protocol_version"] = serde_json::json!("v21");
    h.engine = serde_json::from_value(state).unwrap();
    let mut params = h.engine.params().unwrap().clone();
    params.stress_levels.truncate(1);
    params.toxicity.tiers.clear();
    params.toxicity.horizon_ms = 3_600_000;
    h.operator_admin(AdminCommand::SetParams(params));
    let mut equity = h.engine.markets()[&AAPL].spec.clone();
    equity.price_rules.max_age_ms = 7 * 86_400_000;
    equity.financing.markup_bps = 0;
    h.operator_admin(AdminCommand::UpsertMarket(equity));
    h.operator_admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 0 });
    h
}

pub fn held(long: &str, short: &str) -> Harness {
    let mut h = fixture();
    h.deposit(acct(1), "10000");
    h.deposit(acct(2), "10000");
    h.price(AAPL, "80");
    if f(long).is_positive() {
        assert_fill(&h.market_order(acct(1), AAPL, Side::Buy, long, "80"));
    }
    if f(short).is_positive() {
        assert_fill(&h.market_order(acct(2), AAPL, Side::Sell, short, "80"));
    }
    h
}

pub fn close() -> Millis {
    ny(2026, 10, 20, 16, 0)
}
pub fn open() -> Millis {
    ny(2026, 10, 21, 9, 30)
}

pub fn dividend(id: &str, market: MarketId, gross: &str, rate: &str) -> CorporateAction {
    CorporateAction {
        id: id.into(),
        market,
        previous_close: close(),
        ex_open: open(),
        adjustment: Adjustment::Dividend { gross_per_unit: f(gross), withholding_rate: f(rate) },
    }
}

pub fn split(id: &str, new_units: u32, old_units: u32) -> CorporateAction {
    CorporateAction {
        id: id.into(),
        market: AAPL,
        previous_close: close(),
        ex_open: open(),
        adjustment: Adjustment::Split { new_units, old_units },
    }
}

pub fn schedule(h: &mut Harness, action: CorporateAction) -> Vec<Event> {
    h.operate(AdminCommand::ScheduleCorporateAction(action))
}

pub fn accepted(events: &[Event]) {
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::CorporateActionScheduled { .. })), "{events:?}");
}

pub fn rejected(events: &[Event], fragment: &str) {
    assert!(
        events.iter().any(|e| matches!(e, Event::AdminRejected { reason } if reason.contains(fragment))),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, Event::CorporateActionScheduled { .. })), "{events:?}");
}

pub fn assert_fill(events: &[Event]) {
    assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
}

pub fn tick(h: &mut Harness, at: Millis) -> Vec<Event> {
    h.advance_to(at);
    h.cmd(Command::Tick)
}

pub fn payment(events: &[Event], who: u8) -> Fixed {
    events
        .iter()
        .find_map(|e| match e {
            Event::CorporateDividend { account, amount, .. } if *account == acct(who) => Some(*amount),
            _ => None,
        })
        .expect("account has a dividend application event")
}

pub fn restored(engine: &Engine) -> Engine {
    serde_json::from_slice(&serde_json::to_vec(engine).unwrap()).unwrap()
}
